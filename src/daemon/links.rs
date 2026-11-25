//! Connections between helpers in different mobs (`connect_helpers` of a
//! helper of the workspace's mob and a member of a mob its agent created,
//! or of members of two such mobs).
//!
//! - **Why not an edge of one mob.** An edge joins two members of one mob.
//!   The engine binds a member to a peer outside its mob as an external
//!   edge (`PeerTarget::External`, a descriptor of the other's key and
//!   address) and keeps it in the mob's record, but each mob's members
//!   listen on the in-process transport in a namespace of their own
//!   (`mob.<id>`, the realm of the member's build): an edge addressed at
//!   the other's `inproc://` name is listed as a peer and every send on it
//!   fails `peer_not_found_or_not_trusted` (`docs/evidence.md`, "Helpers
//!   connected across mobs").
//! - **How.** So each end is served on a Unix socket of the daemon's own
//!   under the realm (`<realm>/wire/h-<digest>.sock`, 0600 in the 0700
//!   directory the cross-workspace wires use), with the engine's envelope
//!   handler, its key and inbox looked up per connection and an idle end
//!   woken first ([`super::wires::serve_socket`], as `/wire` serves an
//!   agent), and the external edges are addressed at those sockets
//!   (`uds://`), one in each mob. The handler admits only a sender the
//!   receiving member trusts, which is the other end alone.
//! - **Record.** The daemon keeps the pairs it connected in
//!   `<realm>/helper-links.json` ([`Records`]): the engine keeps the edges
//!   across a restart, but not the sockets, and nothing of the engine drops
//!   an external edge when its far member retires or its mob is destroyed
//!   (the edge then stays among the peers, and a send on it fails "peer delivery
//!   outcome is ambiguous").
//! - **Life.** When either end retires (an `assign` helper when it
//!   finishes, any helper stopped or retired) or its mob is destroyed
//!   ([`crate::mobs::Gone`], from the helpers' follower and the host's
//!   after-destroy hook), the survivor's edge is removed
//!   (`PeerTarget::ExternalName`), it is told in a note, the pair leaves
//!   the record and a socket no pair uses any more is closed. At the
//!   daemon's start the record is checked against the mobs ([`Links::reconcile`]):
//!   a pair whose end is gone goes the same way; each other pair's sockets
//!   listen again, and an edge the engine lost is made again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use meerkat_core::comms::{PeerAddress, PeerName, TrustedPeerDescriptor};
use meerkat_mob::{AgentIdentity, PeerTarget};
use meerkat_mob_mcp::MobMcpState;
use serde::{Deserialize, Serialize};

use crate::mobs::{Gone, HelperKey};
#[cfg(test)]
use meerkat_mob::MobId;

/// The file of the pairs under the realm.
pub const LINKS_FILE: &str = "helper-links.json";

/// The source of the notes the connections send.
const NOTE_SOURCE: &str = "rkat-tui-connect";

/// How long an end not yet seated in its mob (a fork whose spawn is still
/// building) is waited for.
const SEAT_WAIT: Duration = Duration::from_secs(12);
/// The first pause before looking again; each next doubles.
const SEAT_RECHECK_FIRST: Duration = Duration::from_millis(25);
/// The longest pause between two looks.
const SEAT_RECHECK_MAX: Duration = Duration::from_millis(500);

/// One end of a connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct End {
    /// Its mob and member id.
    pub key: HelperKey,
    /// Its comms name (`<mob>/<role>/<member>`): how `contacts` lists it to
    /// the other end, and the name of the other end's edge to it.
    pub peer: String,
    /// The name its parent knows it by.
    pub name: String,
}

/// Two helpers in different mobs, connected both ways.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Its two ends.
    pub ends: [End; 2],
}

impl Link {
    fn joins(&self, a: &HelperKey, b: &HelperKey) -> bool {
        (&self.ends[0].key == a && &self.ends[1].key == b)
            || (&self.ends[0].key == b && &self.ends[1].key == a)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn touches(&self, key: &HelperKey) -> bool {
        self.ends.iter().any(|end| &end.key == key)
    }
}

/// The pairs the daemon connected, as `<realm>/helper-links.json` holds
/// them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Records {
    /// Each pair once.
    #[serde(default)]
    pub links: Vec<Link>,
}

impl Records {
    /// Read `file` (absent: none).
    ///
    /// # Errors
    ///
    /// The file exists and cannot be read as [`Records`].
    pub fn read(file: &Path) -> Result<Self, String> {
        match std::fs::read(file) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|err| err.to_string()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.to_string()),
        }
    }
}

/// One end's socket: aborted and removed when dropped.
struct Socket {
    path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The note the survivor of a connection reads when its other end left.
#[must_use]
pub fn left_note(gone: &End) -> String {
    format!(
        "`{}`, the helper you were connected with, has left (it finished, was retired or stopped, or its mob was destroyed): it gets no more messages, and `contacts` no longer lists it.",
        gone.name
    )
}

/// The daemon's connections of helpers in different mobs.
pub struct Links {
    state: OnceLock<Weak<MobMcpState>>,
    dir: PathBuf,
    file: PathBuf,
    realm: PathBuf,
    records: parking_lot::Mutex<Records>,
    sockets: parking_lot::Mutex<HashMap<HelperKey, Socket>>,
    /// One change at a time: a connection, an end that left, the start's
    /// check.
    changing: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for Links {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Links")
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

impl Links {
    /// The connections the realm at `realm` records; a record that cannot
    /// be read is reported (`Some` beside the value: the daemon starts with
    /// none and says so).
    #[must_use]
    pub fn open(realm: &Path) -> (Arc<Self>, Option<String>) {
        let file = realm.join(LINKS_FILE);
        let (records, problem) = match Records::read(&file) {
            Ok(records) => (records, None),
            Err(why) => (
                Records::default(),
                Some(format!(
                    "the connections of helpers in different mobs in {} cannot be read ({why}): this run starts with none (connect_helpers makes them again)",
                    file.display()
                )),
            ),
        };
        let links = Arc::new(Self {
            state: OnceLock::new(),
            dir: realm.join(super::wires::SOCKET_DIR),
            file,
            realm: realm.to_path_buf(),
            records: parking_lot::Mutex::new(records),
            sockets: parking_lot::Mutex::new(HashMap::new()),
            changing: tokio::sync::Mutex::new(()),
        });
        (links, problem)
    }

    /// Connect through `state` (once; it is made after the tools).
    pub fn bind(&self, state: &Arc<MobMcpState>) {
        let _ = self.state.set(Arc::downgrade(state));
    }

    fn state(&self) -> Result<Arc<MobMcpState>, String> {
        self.state
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| "the daemon is stopping".to_string())
    }

    /// The pairs recorded now.
    #[must_use]
    pub fn links(&self) -> Vec<Link> {
        self.records.lock().links.clone()
    }

    /// The socket path of the end `key`: a digest of its key, so any mob
    /// and member id fits a socket path.
    #[must_use]
    pub fn socket_path(&self, key: &HelperKey) -> PathBuf {
        let digest = crate::hex::sha256(format!("{}\n{}", key.mob, key.member).as_bytes());
        self.dir
            .join(format!("h-{}.sock", digest.get(..16).unwrap_or(&digest)))
    }

    /// Whether `key` has a listening socket now.
    #[must_use]
    pub fn serves(&self, key: &HelperKey) -> bool {
        self.sockets.lock().contains_key(key)
    }

    /// Connect `a` and `b` both ways: whether they were connected already.
    ///
    /// # Errors
    ///
    /// Why not, in words a model can act on; nothing is left half made.
    pub async fn link(&self, a: End, b: End) -> Result<bool, String> {
        let _changing = self.changing.lock().await;
        let state = self.state()?;
        let recorded = self
            .records
            .lock()
            .links
            .iter()
            .any(|link| link.joins(&a.key, &b.key));
        let a_to_b = has_edge(&state, &a.key, &b.peer).await;
        let b_to_a = has_edge(&state, &b.key, &a.peer).await;
        if recorded && a_to_b && b_to_a && self.serves(&a.key) && self.serves(&b.key) {
            return Ok(true);
        }
        let made = self.make(&state, &a, &b, (recorded, a_to_b, b_to_a)).await;
        // A pair not made leaves no socket it opened: only
        // the recorded pairs' ends stay served.
        if made.is_err() {
            self.close_unused();
        }
        made
    }

    /// [`Self::link`]'s work past its check: the sockets, the edges the
    /// engine lacks (`had`: recorded, a to b, b to a) and the record.
    async fn make(
        &self,
        state: &Arc<MobMcpState>,
        a: &End,
        b: &End,
        had: (bool, bool, bool),
    ) -> Result<bool, String> {
        let (recorded, a_to_b, b_to_a) = had;
        let state = Arc::clone(state);
        let a_at = self.listen(&state, &a.key)?;
        let b_at = self.listen(&state, &b.key)?;
        let to_a = descriptor_when_seated(&state, a, &a_at).await?;
        let to_b = descriptor_when_seated(&state, b, &b_at).await?;
        if !a_to_b {
            state
                .mob_wire(&a.key.mob, a.key.member.clone(), PeerTarget::External(to_b))
                .await
                .map_err(|err| format!("the engine refused the edge from `{}`: {err}", a.name))?;
        }
        if !b_to_a
            && let Err(err) = state
                .mob_wire(&b.key.mob, b.key.member.clone(), PeerTarget::External(to_a))
                .await
        {
            if !a_to_b {
                unwire(&state, &a.key, &b.peer).await;
            }
            return Err(format!(
                "the engine refused the edge from `{}`: {err}",
                b.name
            ));
        }
        if !recorded {
            self.records.lock().links.push(Link {
                ends: [a.clone(), b.clone()],
            });
            if let Err(err) = self.save() {
                self.records
                    .lock()
                    .links
                    .retain(|link| !link.joins(&a.key, &b.key));
                unwire(&state, &a.key, &b.peer).await;
                unwire(&state, &b.key, &a.peer).await;
                return Err(err);
            }
        }
        tracing::info!(a = %a.key, b = %b.key, "connected helpers of different mobs");
        Ok(false)
    }

    /// `gone` left for good: each pair it ends goes ([`Self::drop_links`]).
    pub async fn gone(&self, gone: &Gone) {
        let _changing = self.changing.lock().await;
        let Ok(state) = self.state() else {
            return;
        };
        let left = |key: &HelperKey| match gone {
            Gone::Member(member) => member == key,
            Gone::Mob(mob) => &key.mob == mob,
        };
        self.drop_links(&state, &left).await;
    }

    /// At the daemon's start: each pair whose end is no member of its mob
    /// any more goes ([`Self::drop_links`]); each other pair's sockets
    /// listen again, and an edge of it the engine lost is made again.
    pub async fn reconcile(&self) {
        let _changing = self.changing.lock().await;
        let Ok(state) = self.state() else {
            return;
        };
        let links = self.links();
        let mut absent = Vec::new();
        for link in &links {
            for end in &link.ends {
                if !absent.contains(&end.key) && !is_member(&state, &end.key).await {
                    absent.push(end.key.clone());
                }
            }
        }
        self.drop_links(&state, &|key: &HelperKey| absent.contains(key))
            .await;
        for link in self.links() {
            let [a, b] = &link.ends;
            for (from, to) in [(a, b), (b, a)] {
                let at = match self.listen(&state, &to.key) {
                    Ok(at) => at,
                    Err(err) => {
                        tracing::warn!(end = %to.key, error = %err, "a connected helper's socket does not listen");
                        continue;
                    }
                };
                if has_edge(&state, &from.key, &to.peer).await {
                    continue;
                }
                let made = match descriptor(&state, to, &at).await {
                    Ok(Some(descriptor)) => state
                        .mob_wire(
                            &from.key.mob,
                            from.key.member.clone(),
                            PeerTarget::External(descriptor),
                        )
                        .await
                        .map_err(|err| err.to_string()),
                    Ok(None) => Err("it has no comms endpoint".to_string()),
                    Err(err) => Err(err),
                };
                if let Err(err) = made {
                    tracing::warn!(from = %from.key, to = %to.key, error = %err, "a connection's edge was not made again");
                }
            }
        }
    }

    /// Close every socket (the daemon stops).
    pub fn close(&self) {
        self.sockets.lock().clear();
    }

    /// Follow `gone` until it closes.
    pub fn follow(
        self: &Arc<Self>,
        mut gone: tokio::sync::mpsc::UnboundedReceiver<Gone>,
    ) -> tokio::task::JoinHandle<()> {
        let links = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(left) = gone.recv().await {
                let Some(links) = links.upgrade() else {
                    return;
                };
                links.gone(&left).await;
            }
        })
    }

    /// Drop each pair with an end `left` says left: the survivor's edge to
    /// it goes and the survivor is told, the pair leaves the record, and a
    /// socket no pair uses any more is closed.
    async fn drop_links(&self, state: &MobMcpState, left: &(dyn Fn(&HelperKey) -> bool + Sync)) {
        let ending: Vec<Link> = {
            let mut records = self.records.lock();
            let (ending, kept) = std::mem::take(&mut records.links)
                .into_iter()
                .partition(|link| link.ends.iter().any(|end| left(&end.key)));
            records.links = kept;
            ending
        };
        if ending.is_empty() {
            return;
        }
        for link in &ending {
            let [a, b] = &link.ends;
            for (end, other) in [(a, b), (b, a)] {
                if left(&end.key) {
                    // Its own edge, where its mob still holds it.
                    unwire(state, &end.key, &other.peer).await;
                    continue;
                }
                unwire(state, &end.key, &other.peer).await;
                if let Err(err) = super::agents::append_note_to(
                    state,
                    &end.key.mob,
                    &end.key.member,
                    NOTE_SOURCE,
                    left_note(other),
                )
                .await
                {
                    tracing::debug!(helper = %end.key, error = %err, "a connected helper was not told its peer left");
                }
            }
            tracing::info!(a = %a.key, b = %b.key, "a connection of helpers of different mobs ended");
        }
        if let Err(err) = self.save() {
            tracing::warn!(error = %err, "the ended connections stay recorded until the next start");
        }
        self.close_unused();
    }

    /// `key`'s socket, listening (made now unless it is).
    fn listen(&self, state: &Arc<MobMcpState>, key: &HelperKey) -> Result<PathBuf, String> {
        let path = self.socket_path(key);
        let mut sockets = self.sockets.lock();
        if sockets.contains_key(key) {
            return Ok(path);
        }
        let task = super::wires::serve_socket(
            &self.dir,
            &path,
            Arc::downgrade(state),
            key.mob.clone(),
            key.member.clone(),
        )
        .map_err(|err| err.to_string())?;
        sockets.insert(
            key.clone(),
            Socket {
                path: path.clone(),
                task,
            },
        );
        Ok(path)
    }

    /// Close the sockets of ends no recorded pair has.
    fn close_unused(&self) {
        let used: Vec<HelperKey> = self
            .records
            .lock()
            .links
            .iter()
            .flat_map(|link| link.ends.iter().map(|end| end.key.clone()))
            .collect();
        self.sockets.lock().retain(|key, _| used.contains(key));
    }

    fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&*self.records.lock()).map_err(|err| {
            format!(
                "cannot save the connections in {}: {err}",
                self.file.display()
            )
        })?;
        crate::paths::write_atomic(
            &self.file,
            &bytes,
            crate::paths::FileMode::Private,
            Some(&self.realm),
        )
        .map_err(|err| {
            format!(
                "cannot save the connections in {}: {err}",
                self.file.display()
            )
        })
    }

    /// Tests: whether the record holds a pair of `a` and `b`.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn joins(&self, a: &HelperKey, b: &HelperKey) -> bool {
        self.records
            .lock()
            .links
            .iter()
            .any(|link| link.joins(a, b))
    }

    /// Tests: whether any recorded pair has the end `key`.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn has_end(&self, key: &HelperKey) -> bool {
        self.records
            .lock()
            .links
            .iter()
            .any(|link| link.touches(key))
    }
}

/// Whether `key` is a member of its mob now.
async fn is_member(state: &MobMcpState, key: &HelperKey) -> bool {
    let Ok(handle) = state.handle_for(&key.mob).await else {
        return false;
    };
    handle
        .list_members()
        .await
        .iter()
        .any(|member| member.agent_identity == key.member)
}

/// Whether `key`'s mob holds its edge to the peer named `peer`.
async fn has_edge(state: &MobMcpState, key: &HelperKey, peer: &str) -> bool {
    let Ok(handle) = state.handle_for(&key.mob).await else {
        return false;
    };
    let peer = AgentIdentity::from(peer);
    handle
        .list_members()
        .await
        .iter()
        .any(|member| member.agent_identity == key.member && member.wired_to.contains(&peer))
}

/// Remove `key`'s edge to the peer named `peer`, where its mob holds one
/// (best effort: a member or mob that is gone holds none).
async fn unwire(state: &MobMcpState, key: &HelperKey, peer: &str) {
    if !has_edge(state, key, peer).await {
        return;
    }
    let Ok(name) = PeerName::new(peer) else {
        return;
    };
    if let Err(err) = state
        .mob_unwire(&key.mob, key.member.clone(), PeerTarget::ExternalName(name))
        .await
    {
        tracing::warn!(helper = %key, %peer, error = %err, "a connected helper's edge was not removed");
    }
}

/// The descriptor of `end` addressed at its socket `at`, once it is
/// seated (`None`: it has no comms endpoint).
async fn descriptor(
    state: &MobMcpState,
    end: &End,
    at: &Path,
) -> Result<Option<TrustedPeerDescriptor>, String> {
    let handle = state
        .handle_for(&end.key.mob)
        .await
        .map_err(|err| format!("the mob `{}` cannot be reached: {err}", end.key.mob))?;
    let Some(endpoint) = handle
        .member_peer_endpoint(&end.key.member)
        .await
        .map_err(|err| err.to_string())?
    else {
        return Ok(None);
    };
    let mut descriptor = endpoint.descriptor;
    descriptor.address = PeerAddress::parse(format!("uds://{}", at.display()))
        .map_err(|err| format!("the socket {} is no peer address: {err}", at.display()))?;
    descriptor.name = PeerName::new(end.peer.clone())?;
    Ok(Some(descriptor))
}

/// Why one try of [`until_seated`] did not succeed.
pub(super) enum Unseated<W> {
    /// What it needs is not seated in its mob yet (`W` says which): it is
    /// tried again.
    NotYet(W),
    /// It failed for good, in these words.
    Failed(String),
}

/// `attempt`, tried again while what it needs is not seated in its mob
/// yet (a fork whose spawn is still building is listed only later), at
/// most [`SEAT_WAIT`] with pauses doubling from [`SEAT_RECHECK_FIRST`] up
/// to [`SEAT_RECHECK_MAX`]; past the wait, the last [`Unseated::NotYet`].
/// It shrinks to one await when lukacf/meerkat#2433 ships.
pub(super) async fn until_seated<T, W, F>(mut attempt: impl FnMut() -> F) -> Result<T, Unseated<W>>
where
    F: std::future::Future<Output = Result<T, Unseated<W>>>,
{
    let deadline = tokio::time::Instant::now() + SEAT_WAIT;
    let mut pause = SEAT_RECHECK_FIRST;
    loop {
        match attempt().await {
            Err(Unseated::NotYet(which)) => {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Err(Unseated::NotYet(which));
                }
                tokio::time::sleep(pause.min(deadline - now)).await;
                pause = (pause * 2).min(SEAT_RECHECK_MAX);
            }
            done => return done,
        }
    }
}

/// [`descriptor`], waiting ([`until_seated`]) for an end not seated yet.
async fn descriptor_when_seated(
    state: &MobMcpState,
    end: &End,
    at: &Path,
) -> Result<TrustedPeerDescriptor, String> {
    let found = until_seated(move || async move {
        match descriptor(state, end, at).await {
            Ok(Some(descriptor)) => Ok(descriptor),
            Ok(None) => Err(Unseated::NotYet(())),
            Err(why) => Err(Unseated::Failed(why)),
        }
    })
    .await;
    found.map_err(|unseated| match unseated {
        Unseated::NotYet(()) => format!(
            "the helper `{}` is not seated in its mob yet (a fork's start can take a few seconds); call `connect_helpers` again in a moment",
            end.name
        ),
        Unseated::Failed(why) => why,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn end(mob: &str, member: &str) -> End {
        End {
            key: HelperKey::new(MobId::from(mob), AgentIdentity::from(member)),
            peer: format!("{mob}/worker/{member}"),
            name: member.to_string(),
        }
    }

    /// The record reads back what was written, an absent file is none, and
    /// a damaged one is an error (the daemon then starts with none).
    #[test]
    fn the_record_round_trips_and_a_damaged_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(LINKS_FILE);
        assert_eq!(Records::read(&file).unwrap(), Records::default());
        let records = Records {
            links: vec![Link {
                ends: [end("tui", "a"), end("team", "w1")],
            }],
        };
        std::fs::write(&file, serde_json::to_vec(&records).unwrap()).unwrap();
        assert_eq!(Records::read(&file).unwrap(), records);
        std::fs::write(&file, b"{").unwrap();
        assert!(Records::read(&file).is_err());
    }

    /// A pair is the same either way round.
    #[test]
    fn a_link_joins_its_ends_either_way() {
        let link = Link {
            ends: [end("tui", "a"), end("team", "w1")],
        };
        let (a, w) = (&link.ends[0].key, &link.ends[1].key);
        assert!(link.joins(a, w) && link.joins(w, a));
        assert!(link.touches(w));
        assert!(!link.joins(a, &end("team", "w2").key));
    }

    /// Any mob and member id gives a socket path of one length.
    #[test]
    fn a_socket_path_is_a_short_digest_of_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let (links, problem) = Links::open(dir.path());
        assert!(problem.is_none());
        let long = end(&"m".repeat(60), &"x".repeat(60)).key;
        let short = end("tui", "a").key;
        let (one, two) = (links.socket_path(&long), links.socket_path(&short));
        assert_ne!(one, two);
        assert_eq!(one.as_os_str().len(), two.as_os_str().len());
        assert!(one.starts_with(dir.path().join(super::super::wires::SOCKET_DIR)));
    }

    /// The note says who left and that it gets no more messages.
    #[test]
    fn the_survivor_is_told_who_left() {
        let note = left_note(&end("team", "w1"));
        assert!(note.starts_with("`w1`, the helper"), "{note}");
        assert!(note.contains("gets no more messages"), "{note}");
        assert!(note.contains("`contacts`"), "{note}");
    }
}
