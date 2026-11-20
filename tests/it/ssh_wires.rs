//! Wires to an agent on another machine over ssh (`/wire ssh://…`),
//! through a stand-in for `ssh` ([`FakeSsh`]): its control calls run the
//! remote side's answer (`rkat_tui::client::wire_remote::answer`) directly
//! against the other daemon's endpoint file, and its links make the two
//! Unix-socket forwards in this process. The other workspace's daemon runs
//! in a rerun of this test (as in `cross_workspace`: the engine's comms
//! registry is process-global), and the two sides work in step through
//! marker files. A real `ssh` against a private `sshd` is
//! `a_real_ssh_wire_carries_messages_both_ways` (ignored).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::common::{self, ScriptedLlmClient, Step, Workspace, args_for, endpoint_file_for};
use crate::rig::{self, Client, Running};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use rkat_tui::daemon::protocol;
use rkat_tui::daemon::remote_wire::{
    LinkRun, LinkSpec, RemoteError, Reply, Request, SshHost, Transport,
};
use rkat_tui::ui::{Effect, Msg, NoticeLevel};
use serde_json::json;

const SIDE: &str = "RKAT_TUI_SSH_WIRE_SIDE";
const MARKERS: &str = "RKAT_TUI_SSH_WIRE_MARKERS";

struct Markers(PathBuf);

impl Markers {
    fn set(&self, name: &str, body: &str) {
        let tmp = self.0.join(format!("{name}.tmp"));
        std::fs::write(&tmp, body).unwrap();
        std::fs::rename(&tmp, self.0.join(name)).unwrap();
    }

    async fn wait(&self, name: &str) -> String {
        let path = self.0.join(name);
        let deadline = Instant::now() + Duration::from_mins(2);
        while Instant::now() < deadline {
            if let Ok(body) = std::fs::read_to_string(&path) {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the other side never set {name}");
    }
}

/// The stand-in for `ssh`: control calls answered as the remote's
/// `rkat-tui --wire-remote` would, links forwarded in this process.
pub struct FakeSsh {
    /// The remote machine's daemons' endpoint files.
    remote: Vec<PathBuf>,
    /// Whether the host can be reached.
    up: AtomicBool,
    /// Each live link's cut.
    links: Mutex<Vec<tokio::sync::oneshot::Sender<()>>>,
    /// The ops called, in order.
    pub ops: Mutex<Vec<String>>,
}

impl FakeSsh {
    pub fn new(remote: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            remote,
            up: AtomicBool::new(true),
            links: Mutex::new(Vec::new()),
            ops: Mutex::new(Vec::new()),
        })
    }

    /// The host goes away (every link drops, none can be made) or comes
    /// back.
    pub fn reachable(&self, up: bool) {
        self.up.store(up, Ordering::SeqCst);
        if !up {
            for cut in self.links.lock().drain(..) {
                let _ = cut.send(());
            }
        }
    }

    fn unreachable(host: &SshHost) -> RemoteError {
        RemoteError::Ssh {
            host: host.to_string(),
            said: "ssh: connect to host box port 22: Connection refused".into(),
        }
    }
}

/// Serve `listen`, a fresh socket, by connecting each connection to
/// `target`.
fn forward(listen: &Path, target: PathBuf) -> tokio::task::JoinHandle<()> {
    let _ = std::fs::remove_file(listen);
    let listener = tokio::net::UnixListener::bind(listen).unwrap();
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = listener.accept().await {
            let target = target.clone();
            tokio::spawn(async move {
                if let Ok(mut outbound) = tokio::net::UnixStream::connect(&target).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    })
}

struct FakeLink {
    cut: tokio::sync::oneshot::Receiver<()>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    sockets: Vec<PathBuf>,
}

impl LinkRun for FakeLink {
    fn ended(self: Box<Self>) -> BoxFuture<'static, String> {
        let mut this = *self;
        Box::pin(async move {
            let cut = std::mem::replace(&mut this.cut, tokio::sync::oneshot::channel().1);
            let _ = cut.await;
            drop(this);
            "Connection to box closed by remote host.".to_string()
        })
    }
}

impl Drop for FakeLink {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for socket in &self.sockets {
            let _ = std::fs::remove_file(socket);
        }
    }
}

impl Transport for FakeSsh {
    fn call(
        &self,
        host: &SshHost,
        _program: &str,
        request: Request,
    ) -> BoxFuture<'_, Result<Reply, RemoteError>> {
        let host = host.clone();
        Box::pin(async move {
            let op = serde_json::to_value(&request).unwrap()["op"]
                .as_str()
                .unwrap()
                .to_string();
            self.ops.lock().push(op);
            if !self.up.load(Ordering::SeqCst) {
                return Err(Self::unreachable(&host));
            }
            Ok(rkat_tui::client::wire_remote::answer(request, &self.remote).await)
        })
    }

    fn link(
        &self,
        spec: LinkSpec,
        _remote_limit: usize,
    ) -> BoxFuture<'_, Result<Box<dyn LinkRun>, RemoteError>> {
        Box::pin(async move {
            self.ops.lock().push("link".into());
            if !self.up.load(Ordering::SeqCst) {
                return Err(Self::unreachable(&spec.host));
            }
            let (cut_tx, cut) = tokio::sync::oneshot::channel();
            self.links.lock().push(cut_tx);
            let tasks = vec![
                forward(&spec.local_forward, spec.remote_target.clone()),
                forward(&spec.remote_forward, spec.local_target.clone()),
            ];
            Ok(Box::new(FakeLink {
                cut,
                tasks,
                sockets: vec![spec.local_forward, spec.remote_forward],
            }) as Box<dyn LinkRun>)
        })
    }
}

/// An agent's script: a prompt `SEND <words>` sends `<tag>: <words>` to
/// the peer id `to` holds; after the result it says `sent`; anything else
/// it answers `woke`.
fn script(to: &Arc<Mutex<String>>, tag: &'static str) -> Vec<Step> {
    let to = Arc::clone(to);
    let step = Step::Dynamic(Arc::new(move |messages: &[meerkat_core::Message]| {
        if common::answers(messages, Some("m1")) {
            return Step::Text("sent");
        }
        let from = messages
            .iter()
            .rposition(|m| matches!(m, meerkat_core::Message::BlockAssistant(_)))
            .map_or(0, |at| at + 1);
        let prompt = common::user_texts(&messages[from..])
            .into_iter()
            .find(|text| text.starts_with("SEND"));
        match prompt {
            Some(prompt) => Step::Tool {
                id: "m1",
                name: "send_message",
                args: json!({"peer_id": to.lock().clone(), "body": format!("{tag}: {}", prompt.trim_start_matches("SEND").trim()), "handling_mode": "queue"}),
            },
            None => Step::Text("woke"),
        }
    }));
    vec![step; 80]
}

fn last_result(llm: &ScriptedLlmClient) -> String {
    llm.requests()
        .iter()
        .rev()
        .find_map(|messages| {
            messages.iter().rev().find_map(|m| match m {
                meerkat_core::Message::ToolResults { results, .. } => Some(
                    results
                        .iter()
                        .map(meerkat_core::ToolResult::text_content)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
        })
        .unwrap_or_default()
}

/// Wait until `llm` was shown a peer message holding `text`.
async fn shown(llm: &ScriptedLlmClient, text: &str) {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        if format!("{:?}", llm.requests()).contains(text) {
            return;
        }
        assert!(Instant::now() < deadline, "never shown {text:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn turn(daemon: &Running, prompt: &str) {
    let connection = daemon.connect_bound().await;
    daemon.prompt(&connection, prompt).await;
    daemon.idle(&daemon.agent).await;
}

async fn peer_id(daemon: &Running) -> String {
    let connection = daemon.connect_bound().await;
    rkat_tui::client::remote::call::<protocol::method::Card>(
        &connection,
        &protocol::WireAgentParams::default(),
    )
    .await
    .unwrap()
    .peer_id
}

fn workspace(name: &str) -> (Workspace, PathBuf) {
    let ws = Workspace::new();
    let root = ws.sibling(name);
    std::fs::create_dir_all(&root).unwrap();
    (ws, root)
}

async fn client(root: &Path) -> Client {
    let mut config = rig::client_config(
        &args_for(root),
        None,
        common::no_lists(),
        &endpoint_file_for(root),
        None,
    )
    .await;
    config.wire_files = Some(Vec::new());
    Client::attach_with_config(config).await
}

async fn notice(client: &mut Client, text: &str) -> (NoticeLevel, String) {
    match client
        .wait_for(|msg| matches!(msg, Msg::Notice { text: t, .. } if t.contains(text)))
        .await
    {
        Msg::Notice { level, text } => (level, text),
        _ => unreachable!(),
    }
}

/// The agent's wires as its daemon lists them: label, host, link state.
async fn wires_of(daemon: &Running) -> Vec<(String, Option<String>, Option<String>)> {
    let status = daemon.status().await;
    status
        .agents
        .iter()
        .find(|row| row.name == daemon.agent)
        .map(|row| {
            row.wires
                .iter()
                .map(|w| (w.label(), w.host.clone(), w.link.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until `daemon`'s wire shows its link as `state`.
async fn link_is(daemon: &Running, state: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let wires = wires_of(daemon).await;
        if wires.iter().any(|w| w.2.as_deref() == Some(state)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the link never was {state}: {wires:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The daemon of `root` reaching other machines through `ssh`.
async fn start_with_ssh(root: &Path, llm: &Arc<ScriptedLlmClient>, ssh: &Arc<FakeSsh>) -> Running {
    let options = Running::options(root, llm).with_transport(Arc::clone(ssh) as _);
    Running::start_with(&args_for(root), options).await
}

/// The remote machine's side: its daemon, its model, its sends.
async fn remote_side(markers: Markers) {
    let (_ws, root) = workspace("beta");
    let to_alpha = Arc::new(Mutex::new(String::new()));
    let llm = ScriptedLlmClient::new(script(&to_alpha, "from B"));
    let b = Running::start(&root, &llm).await;
    markers.set(
        "b.ready",
        &format!("{}\n{}", b.endpoint_file.display(), b.agent),
    );
    *to_alpha.lock() = markers.wait("a.peer").await;
    let a_agent = markers.wait("a.agent").await;
    shown(
        &llm,
        &format!("Peer message from alpha/{a_agent}:\\nfrom A: hello"),
    )
    .await;
    b.idle(&b.agent).await;
    let wires = wires_of(&b).await;
    assert_eq!(wires.len(), 1, "{wires:?}");
    assert_eq!(wires[0].0, format!("alpha/{a_agent}"));
    assert_eq!(
        wires[0].1, None,
        "the remote side's wire is a plain local one"
    );
    turn(&b, "SEND reply").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.set("b.replied", "");

    markers.wait("b.send_down").await;
    turn(&b, "SEND whiledown").await;
    assert!(
        !last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.set("b.sent_down", "");

    markers.wait("b.again").await;
    turn(&b, "SEND again").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.set("b.again_ok", "");

    markers.wait("unwired").await;
    assert!(wires_of(&b).await.is_empty());
    markers.set("b.done", "");
    b.stop().await;
}

/// `/wire ssh://box/beta/<agent>`: the card exchange and the link through
/// the stand-in, a message each way, the link down (the user told once,
/// a send refused with the words, the remote's send not delivered), back
/// (told once, sends delivered), this daemon restarted (the link made
/// again), and `/unwire` on both sides.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::disallowed_methods)] // test-only process launch: the other side
async fn an_ssh_wire_carries_messages_both_ways_and_survives_drops_and_restarts() {
    if std::env::var(SIDE).as_deref() == Ok("remote") {
        remote_side(Markers(PathBuf::from(std::env::var(MARKERS).unwrap()))).await;
        return;
    }
    let (ws, root) = workspace("alpha");
    let markers_dir = ws.sibling("markers");
    std::fs::create_dir_all(&markers_dir).unwrap();
    let mut other = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ssh_wires::an_ssh_wire_carries_messages_both_ways_and_survives_drops_and_restarts",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(SIDE, "remote")
        .env(MARKERS, &markers_dir)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let markers = Markers(markers_dir);
    let ready = markers.wait("b.ready").await;
    let (b_file, b_agent) = ready.split_once('\n').unwrap();
    let b_file = PathBuf::from(b_file);
    let b_label = format!("beta/{b_agent}");

    let ssh = FakeSsh::new(vec![b_file.clone()]);
    let to_beta = Arc::new(Mutex::new(String::new()));
    let llm = ScriptedLlmClient::new(script(&to_beta, "from A"));
    let a = start_with_ssh(&root, &llm, &ssh).await;
    let mut c = client(&root).await;

    // A host alone lists the agents there.
    c.send(Effect::Wire(Some("ssh://box".into())));
    let (_, listed) = notice(&mut c, "agents on box").await;
    assert!(listed.contains(&b_label), "{listed}");

    c.send(Effect::Wire(Some(format!("ssh://box/beta/{b_agent}"))));
    let a_label = format!("alpha/{}", a.agent);
    let (level, text) = notice(&mut c, "wired ").await;
    assert_eq!(level, NoticeLevel::Info, "{text}");
    assert!(
        text.contains(&format!("wired {a_label} and {b_label} over ssh to box")),
        "{text}"
    );
    assert_eq!(
        wires_of(&a).await,
        vec![(b_label.clone(), Some("box".into()), Some("up".into()))]
    );
    assert_eq!(
        *ssh.ops.lock(),
        ["list", "card", "link", "wire"],
        "the card exchange and one link"
    );

    // A to B, and back.
    *to_beta.lock() = {
        let endpoint = rkat_tui::daemon::endpoint::Endpoint::read(&b_file).unwrap();
        let (probe, _) = rkat_tui::client::transport::Connection::attach_as(
            &endpoint,
            Duration::from_secs(5),
            None,
            true,
        )
        .await
        .unwrap();
        rkat_tui::client::remote::call::<protocol::method::Card>(
            &probe,
            &protocol::WireAgentParams {
                agent: Some(b_agent.to_string()),
            },
        )
        .await
        .unwrap()
        .peer_id
    };
    markers.set("a.peer", &peer_id(&a).await);
    markers.set("a.agent", &a.agent);
    turn(&a, "SEND hello").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.wait("b.replied").await;
    shown(
        &llm,
        &format!("Peer message from {b_label}:\\nfrom B: reply"),
    )
    .await;
    a.idle(&a.agent).await;

    // The link drops: the user is told once; a send is refused with the
    // words; the remote's send is not delivered.
    ssh.reachable(false);
    let (level, down) = notice(&mut c, "the link to box is down").await;
    assert_eq!(level, NoticeLevel::Warn, "{down}");
    link_is(&a, "down").await;
    turn(&a, "SEND whiledown").await;
    let refused = last_result(&llm);
    assert!(
        refused.contains(&format!(
            "`{b_label}` cannot get messages now: the link to box is down"
        )) && refused.contains("try again later, or tell the user"),
        "{refused}"
    );
    markers.set("b.send_down", "");
    markers.wait("b.sent_down").await;
    assert!(
        !format!("{:?}", llm.requests()).contains("from B: whiledown"),
        "a message sent while the link was down arrived"
    );
    // Back: told once, and both ways work again.
    ssh.reachable(true);
    notice(&mut c, "the link to box is back").await;
    link_is(&a, "up").await;
    let downs = c
        .seen
        .iter()
        .filter(
            |m| matches!(m, Msg::Notice { text, .. } if text.contains("the link to box is down")),
        )
        .count();
    assert_eq!(downs, 1, "told of the drop once: {}", c.notices());
    turn(&a, "SEND afterback").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.set("b.again", "");
    markers.wait("b.again_ok").await;
    shown(&llm, "from B: again").await;
    a.idle(&a.agent).await;

    // This daemon restarts: the stored wire's link is made again.
    c.quit().await;
    a.stop().await;
    let llm = ScriptedLlmClient::new(script(&to_beta, "from A2"));
    let a = start_with_ssh(&root, &llm, &ssh).await;
    link_is(&a, "up").await;
    turn(&a, "SEND afterrestart").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );

    // `/unwire`: both halves, through ssh for the remote one.
    let mut c = client(&root).await;
    c.send(Effect::Unwire(Some(b_label.clone())));
    let (_, text) = notice(&mut c, "unwired").await;
    assert_eq!(text, format!("unwired {b_label} on both sides"));
    assert!(wires_of(&a).await.is_empty());
    assert_eq!(ssh.ops.lock().last().map(String::as_str), Some("unwire"));
    markers.set("unwired", "");
    markers.wait("b.done").await;
    c.quit().await;
    a.stop().await;
    let status = tokio::time::timeout(Duration::from_mins(2), other.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "the other side failed: {status}");
}

/// A host that cannot be reached: nothing is wired, and the words say to
/// make `ssh <host> true` work; no daemon there says to start rkat-tui
/// there; a host ssh could read as an option is refused before any launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ssh_wire_that_cannot_be_made_says_why() {
    let (_ws, root) = workspace("alpha");
    let ssh = FakeSsh::new(Vec::new());
    let llm = ScriptedLlmClient::new(vec![Step::Text("idle")]);
    let options = Running::options(&root, &llm).with_transport(Arc::clone(&ssh) as _);
    let a = Running::start_with(&args_for(&root), options).await;
    let mut c = client(&root).await;
    // No daemon there.
    c.send(Effect::Wire(Some("ssh://box/beta/main-ab12".into())));
    let (level, text) = notice(&mut c, "nothing was wired").await;
    assert_eq!(level, NoticeLevel::Warn);
    assert!(
        text.contains("start rkat-tui in that workspace there first"),
        "{text}"
    );
    // The host down.
    ssh.reachable(false);
    c.send(Effect::Wire(Some("ssh://box/beta/main-ab12".into())));
    let (_, text) = notice(&mut c, "nothing was wired").await;
    assert!(text.contains("make `ssh box true` work first"), "{text}");
    // A host ssh could read as an option.
    c.send(Effect::Wire(Some(
        "ssh://-oProxyCommand=x/beta/main-ab12".into(),
    )));
    let (_, text) = notice(&mut c, "nothing was wired").await;
    assert!(text.contains("is no ssh host"), "{text}");
    assert!(
        !ssh.ops.lock().iter().any(|op| op == "link"),
        "{:?}",
        ssh.ops.lock()
    );
    c.quit().await;
    a.stop().await;
}

/// Set in the real-ssh rerun: the other side's state home.
const STATE: &str = "RKAT_TUI_SSH_WIRE_STATE";

/// The real-ssh run's remote side: a daemon whose endpoint file is in the
/// state home the private sshd's sessions get, so `rkat-tui
/// --wire-remote` there finds it.
async fn real_remote_side(markers: Markers, state: PathBuf) {
    let (_ws, root) = workspace("beta");
    let to_alpha = Arc::new(Mutex::new(String::new()));
    let llm = ScriptedLlmClient::new(script(&to_alpha, "from B"));
    let mut options = Running::options(&root, &llm);
    options.endpoint_file = None;
    let b = Running::start_with(&args_for(&root), options.with_state_home(state)).await;
    markers.set("b.ready", &b.agent);
    *to_alpha.lock() = markers.wait("a.peer").await;
    shown(&llm, "from A: hello").await;
    b.idle(&b.agent).await;
    turn(&b, "SEND reply").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.set("b.replied", "");
    markers.wait("unwired").await;
    assert!(wires_of(&b).await.is_empty());
    markers.set("b.done", "");
    b.stop().await;
}

/// Run `program` with `args` to its end (key generation).
#[allow(clippy::disallowed_methods)] // test-only process launch
fn run(program: &str, args: &[&str]) {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "{program} {args:?}: {status}");
}

/// A real `ssh` to a private `sshd` of this user on a loopback port (its
/// own host key, `authorized_keys` and client key, all in a temporary
/// directory; `~/.ssh` is never read or written): `/wire ssh://box/…`
/// with `--remote-rkat-tui` naming this build's binary, a message each
/// way, `/unwire`. Ignored: it needs `/usr/sbin/sshd` and `ssh-keygen`.
/// `cargo nextest run --all-features --test it --run-ignored only -E
/// 'test(a_real_ssh_wire)'`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "starts a private sshd; run by hand"]
#[allow(clippy::disallowed_methods)] // test-only process launches: sshd, the other side
async fn a_real_ssh_wire_carries_messages_both_ways() {
    if std::env::var(SIDE).as_deref() == Ok("real-remote") {
        real_remote_side(
            Markers(PathBuf::from(std::env::var(MARKERS).unwrap())),
            PathBuf::from(std::env::var(STATE).unwrap()),
        )
        .await;
        return;
    }
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let d = dir.path();
    let path = |name: &str| d.join(name).display().to_string();
    let state = d.join("state");
    std::fs::create_dir_all(&state).unwrap();
    run(
        "ssh-keygen",
        &["-q", "-t", "ed25519", "-N", "", "-f", &path("host_key")],
    );
    run(
        "ssh-keygen",
        &["-q", "-t", "ed25519", "-N", "", "-f", &path("client_key")],
    );
    std::fs::copy(d.join("client_key.pub"), d.join("authorized_keys")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let user = std::env::var("USER").unwrap();
    std::fs::write(
        d.join("sshd_config"),
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nAllowStreamLocalForwarding yes\nStreamLocalBindUnlink yes\nPrintMotd no\nSetEnv XDG_STATE_HOME={} XDG_RUNTIME_DIR=\n",
            path("host_key"),
            path("sshd.pid"),
            path("authorized_keys"),
            state.display()
        ),
    )
    .unwrap();
    std::fs::write(
        d.join("ssh_config"),
        format!(
            "Host box\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  IdentityFile {}\n  IdentitiesOnly yes\n  IdentityAgent none\n  UserKnownHostsFile {}\n  GlobalKnownHostsFile /dev/null\n  StrictHostKeyChecking accept-new\n",
            path("client_key"),
            path("known_hosts")
        ),
    )
    .unwrap();
    let mut sshd = tokio::process::Command::new("/usr/sbin/sshd")
        .args(["-f", &path("sshd_config"), "-D", "-e"])
        .stderr(std::fs::File::create(d.join("sshd.log")).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let sshd_pid = sshd.id().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_err()
    {
        assert!(
            Instant::now() < deadline,
            "sshd did not listen: {}",
            std::fs::read_to_string(d.join("sshd.log")).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let markers_dir = d.join("markers");
    std::fs::create_dir_all(&markers_dir).unwrap();
    let mut other = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ssh_wires::a_real_ssh_wire_carries_messages_both_ways",
            "--nocapture",
            "--test-threads=1",
            "--ignored",
        ])
        .env(SIDE, "real-remote")
        .env(MARKERS, &markers_dir)
        .env(STATE, &state)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let markers = Markers(markers_dir);
    let b_agent = markers.wait("b.ready").await;
    let b_label = format!("beta/{b_agent}");

    let (_ws, root) = workspace("alpha");
    let to_beta = Arc::new(Mutex::new(String::new()));
    let llm = ScriptedLlmClient::new(script(&to_beta, "from A"));
    let ssh = rkat_tui::daemon::remote_wire::Ssh::with(
        "ssh".into(),
        vec!["-F".into(), path("ssh_config")],
    );
    let options = Running::options(&root, &llm).with_transport(Arc::new(ssh));
    let a = Running::start_with(&args_for(&root), options).await;
    let mut c = client(&root).await;
    let bin = env!("CARGO_BIN_EXE_rkat-tui");
    c.send(Effect::Wire(Some(format!(
        "ssh://box/beta/{b_agent} --remote-rkat-tui {bin}"
    ))));
    let (level, text) = notice(&mut c, "wired").await;
    assert_eq!(
        level,
        NoticeLevel::Info,
        "{text}\nsshd: {}",
        std::fs::read_to_string(d.join("sshd.log")).unwrap_or_default()
    );
    assert!(
        text.contains(&format!("and {b_label} over ssh to box")),
        "{text}"
    );
    let wires = wires_of(&a).await;
    if wires != vec![(b_label.clone(), Some("box".into()), Some("up".into()))] {
        tokio::time::sleep(Duration::from_secs(1)).await;
        c.drain();
        panic!(
            "{wires:?}\nnotices: {}\nsshd: {}",
            c.notices(),
            std::fs::read_to_string(d.join("sshd.log")).unwrap_or_default()
        );
    }
    // The remote agent's peer id: from the forwarded socket's card there.
    *to_beta.lock() = {
        let status = a.status().await;
        let row = status.agents.iter().find(|r| r.name == a.agent).unwrap();
        assert_eq!(row.wires.len(), 1);
        let realm = common::realm_of(&root);
        let records: serde_json::Value =
            serde_json::from_slice(&std::fs::read(realm.join("wires.json")).unwrap()).unwrap();
        records["agents"][&a.agent][0]["peer_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    markers.set("a.peer", &peer_id(&a).await);
    turn(&a, "SEND hello").await;
    assert!(
        last_result(&llm).contains("\"acked\""),
        "{}",
        last_result(&llm)
    );
    markers.wait("b.replied").await;
    shown(
        &llm,
        &format!("Peer message from {b_label}:\\nfrom B: reply"),
    )
    .await;
    a.idle(&a.agent).await;

    c.send(Effect::Unwire(Some(b_label.clone())));
    let (_, text) = notice(&mut c, "unwired").await;
    assert_eq!(text, format!("unwired {b_label} on both sides"));
    markers.set("unwired", "");
    markers.wait("b.done").await;
    c.quit().await;
    a.stop().await;
    let status = tokio::time::timeout(Duration::from_mins(2), other.wait())
        .await
        .unwrap()
        .unwrap();
    // The private sshd, by its exact pid.
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(sshd_pid).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    );
    let _ = sshd.wait().await;
    assert!(status.success(), "the other side failed: {status}");
}
