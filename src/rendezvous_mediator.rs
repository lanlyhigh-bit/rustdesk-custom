use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
    time::{Duration, Instant},
};

use uuid::Uuid;

use hbb_common::{
    allow_err,
    anyhow::{self, bail},
    config::{
        self, keys::*, option2bool, use_ws, Config, CONNECT_TIMEOUT, REG_INTERVAL, RENDEZVOUS_PORT,
    },
    futures::future::join_all,
    log,
    protobuf::Message as _,
    rendezvous_proto::*,
    sleep,
    socket_client::{self, connect_tcp, is_ipv4, new_direct_udp_for, new_udp_for},
    tokio::{self, select, sync::Mutex, time::interval},
    udp::FramedSocket,
    AddrMangle, IntoTargetAddr, ResultType, Stream, TargetAddr,
};

use crate::{
    check_port,
    server::{check_zombie, new as new_server, ConnectionMeta, ServerPtr},
};

type Message = RendezvousMessage;

fn connection_meta(
    control_permissions: Option<ControlPermissions>,
    controlled_context: Option<ControlledContext>,
) -> ConnectionMeta {
    ConnectionMeta {
        control_permissions,
        controlled_context,
    }
}

lazy_static::lazy_static! {
    static ref SOLVING_PK_MISMATCH: Mutex<String> = Default::default();
    static ref LAST_MSG: Mutex<(SocketAddr, Instant)> = Mutex::new((SocketAddr::new([0; 4].into(), 0), Instant::now()));
    static ref LAST_RELAY_MSG: Mutex<(SocketAddr, Instant)> = Mutex::new((SocketAddr::new([0; 4].into(), 0), Instant::now()));
}
static SHOULD_EXIT: AtomicBool = AtomicBool::new(false);
static MANUAL_RESTARTED: AtomicBool = AtomicBool::new(false);
static SENT_REGISTER_PK: AtomicBool = AtomicBool::new(false);
// 定制（2026-10-23）：UDP 注册持续失败后自动改走 TCP 注册。hbbs 在 21116 同时
// 监听 TCP，上游 use_ws / 禁 UDP 部署走的就是这条路径，punch/relay 在 TCP 通道
// 同样可用。触发：start_udp 连续 UDP_FAILS_FOR_TCP 次无回复（自建服务器
// reg_timeout 恒为 3s，60 次 ≈ 3 分钟）。真实场景：办公防火墙只放行 TCP、
// IPv6 临时地址轮换后 socket 源失效且 rebind 未自愈——两种情况下 TCP 通常仍通。
static FORCE_TCP_AFTER_UDP_FAIL: AtomicBool = AtomicBool::new(false);
const UDP_FAILS_FOR_TCP: i64 = 60;
const ERR_UDP_FALLBACK: &str = "udp register kept failing, switch to tcp";
// 定制：request_pk 之后允许补发 RegisterPk 的最大次数（每次一个心跳周期）。
const PK_RETRY_MAX: u8 = 5;
pub(crate) static NEEDS_DEPLOY: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "android")]
static NOTIFIED_NEEDS_DEPLOY: AtomicBool = AtomicBool::new(false);
// register_pk retry interval (ms) when device is awaiting deployment
const DEPLOY_RETRY_INTERVAL: i64 = 30_000;
lazy_static::lazy_static! {
    static ref LAST_NOT_DEPLOYED_REGISTER: Mutex<Option<Instant>> = Mutex::new(None);
}

// Single source of truth for the "awaiting deployment" backoff. The server has
// already told us this device is not in its db; until the operator runs
// `rustdesk --deploy --token <api_token>` there is no point re-running the
// register path more often than DEPLOY_RETRY_INTERVAL. Gating in the timer
// loops (rather than only inside register_pk) also avoids the
// last_register_sent / fails / latency / UDP-rebind churn the loop would
// otherwise spin on while no response ever comes back.
async fn deploy_register_throttled() -> bool {
    if !NEEDS_DEPLOY.load(Ordering::SeqCst) {
        return false;
    }
    LAST_NOT_DEPLOYED_REGISTER
        .lock()
        .await
        .map(|t| (t.elapsed().as_millis() as i64) < DEPLOY_RETRY_INTERVAL)
        .unwrap_or(false)
}

#[cfg(target_os = "android")]
fn notify_android_needs_deploy() {
    if NOTIFIED_NEEDS_DEPLOY.load(Ordering::SeqCst) {
        return;
    }
    let event = serde_json::json!({ "name": "android_needs_deploy" }).to_string();
    if matches!(
        crate::flutter::push_global_event(crate::flutter::APP_TYPE_MAIN, event),
        Some(true)
    ) {
        NOTIFIED_NEEDS_DEPLOY.store(true, Ordering::SeqCst);
    }
}

#[cfg(target_os = "android")]
pub(crate) fn reset_needs_deploy_notification() {
    NEEDS_DEPLOY.store(false, Ordering::SeqCst);
    NOTIFIED_NEEDS_DEPLOY.store(false, Ordering::SeqCst);
}

#[derive(Clone)]
pub struct RendezvousMediator {
    addr: TargetAddr<'static>,
    host: String,
    host_prefix: String,
    keep_alive: i32,
    // 定制（2026-10-26）：待重发的 RegisterPk 次数。上游 hbbs 在 ip_change
    // （客户端切换网络路径，例如断开 WG 改走办公网 IPv6）时，update_addr 不会
    // 更新 last_reg_time，只回 request_pk=true——服务器的在线计时器此后完全
    // 依赖这一次 RegisterPk 触发的 update_pk 续期。而上游只发一次 RegisterPk：
    // 若它恰好在路由收敛的瞬间丢失/被拒，socket_addr 就永远停留在旧 IP，
    // 之后每次 RegisterPeer 都被判 ip_change → 永远 request_pk=true →
    // last_reg_time 永不刷新（30s 后 punch 回 OFFLINE）——表现为「自己显示在线、
    // 别人却连不上」。这里把它改成可重发，确保握手最终完成。
    pk_retry: u8,
}

impl RendezvousMediator {
    pub fn restart() {
        SHOULD_EXIT.store(true, Ordering::SeqCst);
        MANUAL_RESTARTED.store(true, Ordering::SeqCst);
        log::info!("server restart");
    }

    pub async fn start_all() {
        crate::test_nat_type();
        if config::is_outgoing_only() {
            loop {
                sleep(1.).await;
            }
        }
        crate::hbbs_http::sync::start();
        #[cfg(target_os = "windows")]
        if crate::platform::is_installed() && crate::is_server() {
            crate::updater::start_auto_update();
        }
        check_zombie();
        let server = new_server();
        if config::option2bool("stop-service", &Config::get_option("stop-service")) {
            crate::test_rendezvous_server();
        }
        let server_cloned = server.clone();
        tokio::spawn(async move {
            direct_server(server_cloned).await;
        });
        #[cfg(target_os = "android")]
        let start_lan_listening = true;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let start_lan_listening = crate::platform::is_installed();
        if start_lan_listening {
            std::thread::spawn(move || {
                allow_err!(super::lan::start_listening());
            });
        }
        scrap::codec::test_av1();
        *LAST_NOT_DEPLOYED_REGISTER.lock().await = None;
        loop {
            let timeout = Arc::new(RwLock::new(CONNECT_TIMEOUT));
            let conn_start_time = Instant::now();
            *SOLVING_PK_MISMATCH.lock().await = "".to_owned();
            if !config::option2bool("stop-service", &Config::get_option("stop-service"))
                && !crate::platform::installing_service()
            {
                let mut futs = Vec::new();
                let servers = Config::get_rendezvous_servers();
                SHOULD_EXIT.store(false, Ordering::SeqCst);
                MANUAL_RESTARTED.store(false, Ordering::SeqCst);
                for host in servers.clone() {
                    let server = server.clone();
                    let timeout = timeout.clone();
                    futs.push(tokio::spawn(async move {
                        if let Err(err) = Self::start(server, host).await {
                            let err = format!("rendezvous mediator error: {err}");
                            // When user reboot, there might be below error, waiting too long
                            // (CONNECT_TIMEOUT 18s) will make user think there is bug
                            if err.contains("10054") || err.contains("11001") {
                                // No such host is known. (os error 11001)
                                // An existing connection was forcibly closed by the remote host. (os error 10054): also happens for UDP
                                *timeout.write().unwrap() = 3000;
                            }
                            log::error!("{err}");
                        }
                        // SHOULD_EXIT here is to ensure once one exits, the others also exit.
                        SHOULD_EXIT.store(true, Ordering::SeqCst);
                    }));
                }
                join_all(futs).await;
            } else {
                server.write().unwrap().close_connections();
            }
            Config::reset_online();
            let timeout = *timeout.read().unwrap();
            if !MANUAL_RESTARTED.load(Ordering::SeqCst) {
                let elapsed = conn_start_time.elapsed().as_millis() as u64;
                if elapsed < timeout {
                    sleep(((timeout - elapsed) / 1000) as _).await;
                }
            } else {
                // https://github.com/rustdesk/rustdesk/issues/12233
                sleep(0.033).await;
            }
        }
    }

    fn get_host_prefix(host: &str) -> String {
        host.split(".")
            .next()
            .map(|x| {
                if x.parse::<i32>().is_ok() {
                    host.to_owned()
                } else {
                    x.to_owned()
                }
            })
            .unwrap_or(host.to_owned())
    }

    pub async fn start_udp(server: ServerPtr, host: String) -> ResultType<()> {
        let host = check_port(&host, RENDEZVOUS_PORT);
        log::info!("start udp: {host}");
        // 定制：同一域名可能同时解析出 A/AAAA，按可达性择优，避免选中不通的那个。
        // rz.host 仍保留原串，供 rebind 与延迟统计使用。
        let target = crate::net_target::pick_reachable(&host).await;
        let (mut socket, mut addr) = new_udp_for(&target, CONNECT_TIMEOUT).await?;
        let mut rz = Self {
            addr: addr.clone(),
            host: host.clone(),
            host_prefix: Self::get_host_prefix(&host),
            keep_alive: crate::DEFAULT_KEEP_ALIVE,
            pk_retry: 0,
        };

        let mut timer = crate::rustdesk_interval(interval(crate::TIMER_OUT));
        const MIN_REG_TIMEOUT: i64 = 3_000;
        const MAX_REG_TIMEOUT: i64 = 30_000;
        let mut reg_timeout = MIN_REG_TIMEOUT;
        const MAX_FAILS1: i64 = 2;
        const MAX_FAILS2: i64 = 4;
        const DNS_INTERVAL: i64 = 60_000;
        let mut fails = 0;
        let mut last_register_resp: Option<Instant> = None;
        let mut last_register_sent: Option<Instant> = None;
        let mut last_dns_check = Instant::now();
        let mut old_latency = 0;
        let mut ema_latency = 0;
        loop {
            let mut update_latency = || {
                last_register_resp = Some(Instant::now());
                fails = 0;
                reg_timeout = MIN_REG_TIMEOUT;
                let mut latency = last_register_sent
                    .map(|x| x.elapsed().as_micros() as i64)
                    .unwrap_or(0);
                last_register_sent = None;
                if latency < 0 || latency > 1_000_000 {
                    return;
                }
                if ema_latency == 0 {
                    ema_latency = latency;
                } else {
                    ema_latency = latency / 30 + (ema_latency * 29 / 30);
                    latency = ema_latency;
                }
                let mut n = latency / 5;
                if n < 3000 {
                    n = 3000;
                }
                if (latency - old_latency).abs() > n || old_latency <= 0 {
                    Config::update_latency(&host, latency);
                    log::debug!("Latency of {}: {}ms", host, latency as f64 / 1000.);
                    old_latency = latency;
                }
            };
            select! {
                n = socket.next() => {
                    match n {
                        Some(Ok((bytes, _))) => {
                            if let Ok(msg) = Message::parse_from_bytes(&bytes) {
                                rz.handle_resp(msg.union, Sink::Framed(&mut socket, &addr), &server, &mut update_latency).await?;
                            } else {
                                log::debug!("Non-protobuf message bytes received: {:?}", bytes);
                            }
                        },
                        Some(Err(e)) => bail!("Failed to receive next: {}", e),  // maybe socks5 tcp disconnected
                        None => {
                            bail!("Socket receive none. Maybe socks5 server is down.");
                        },
                    }
                },
                _ = timer.tick() => {
                    if SHOULD_EXIT.load(Ordering::SeqCst) {
                        break;
                    }
                    // The server already told us this device is not deployed. Skip
                    // the whole register / fails / latency / UDP-rebind path until
                    // DEPLOY_RETRY_INTERVAL elapses, otherwise the loop spins every
                    // few seconds (log spam + misapplied network-recovery rebind)
                    // until the operator runs `rustdesk --deploy`.
                    if deploy_register_throttled().await {
                        continue;
                    }
                    let now = Some(Instant::now());
                    let expired = last_register_resp.map(|x| x.elapsed().as_millis() as i64 >= REG_INTERVAL).unwrap_or(true);
                    let timeout = last_register_sent.map(|x| x.elapsed().as_millis() as i64 >= reg_timeout).unwrap_or(false);
                    // temporarily disable exponential backoff for android before we add wakeup trigger to force connect in android
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    if crate::using_public_server() { // only turn on this for public server, may help DDNS self-hosting user.
                        if timeout && reg_timeout < MAX_REG_TIMEOUT {
                            reg_timeout += MIN_REG_TIMEOUT;
                        }
                    }
                    if timeout || (last_register_sent.is_none() && expired) {
                        if timeout {
                            fails += 1;                            // 定制：保险丝——UDP 连续无回复到阈值即放弃本轮 UDP，
                            // 让 start() 切换 TCP 注册（hbbs 21116/tcp 同样监听注册服务）。
                            if fails >= UDP_FAILS_FOR_TCP {
                                bail!("{}", ERR_UDP_FALLBACK);
                            }
                            if fails >= MAX_FAILS2 {
                                Config::update_latency(&host, -1);
                                old_latency = 0;
                                if last_dns_check.elapsed().as_millis() as i64 > DNS_INTERVAL {
                                    // in some case of network reconnect (dial IP network),
                                    // old UDP socket not work any more after network recover
                                    // 定制：重新择优（网络环境可能已变），而不是沿用解析顺序的第一个。
                                    let rebind_target = crate::net_target::pick_reachable(&rz.host).await;
                                    if let Some((s, new_addr)) = socket_client::rebind_udp_for(&rebind_target).await? {
                                        socket = s;
                                        rz.addr = new_addr.clone();
                                        addr = new_addr;
                                    }
                                    last_dns_check = Instant::now();
                                }
                            } else if fails >= MAX_FAILS1 {
                                Config::update_latency(&host, 0);
                                old_latency = 0;
                            }
                        }
                        // Some firewalls drop the rendezvous reply, so request_pk
                        // never arrives and RegisterPk is never sent. RegisterPk is
                        // one-way: the server writes our address on receipt. Send it
                        // proactively while registration keeps timing out.
                        if timeout && fails >= MAX_FAILS1 && fails % 4 == 0 {
                            rz.register_pk(Sink::Framed(&mut socket, &addr)).await?;
                        }
                        rz.register_peer(Sink::Framed(&mut socket, &addr)).await?;
                        // 定制：request_pk 握手尚未完成（服务器已要求重新登记公钥，
                        // 但 RegisterPk 还没被确认）时，每个注册周期补发一次
                        // RegisterPk，最多 PK_RETRY_MAX 次。上游这里只发一次，
                        // 丢一次就会让服务器端 last_reg_time 永远不续期。
                        if rz.pk_retry > 0 {
                            rz.pk_retry -= 1;
                            rz.register_pk(Sink::Framed(&mut socket, &addr)).await?;
                        }
                        last_register_sent = now;
                    }
                }
            }
        }
        Ok(())
    }

    #[inline]
    async fn handle_resp(
        &mut self,
        msg: Option<rendezvous_message::Union>,
        sink: Sink<'_>,
        server: &ServerPtr,
        update_latency: &mut impl FnMut(),
    ) -> ResultType<()> {
        match msg {
            Some(rendezvous_message::Union::RegisterPeerResponse(rpr)) => {
                update_latency();
                if rpr.request_pk {
                    log::info!("request_pk received from {}", self.host);
                    // 定制：登记补发次数（见 RendezvousMediator::pk_retry 注释）。
                    // 上游这里只发一次 RegisterPk，一旦丢失，服务器端 last_reg_time
                    // 永不续期（自己显示在线、别人连不上）。由主循环补发兜底。
                    self.pk_retry = PK_RETRY_MAX;
                    self.register_pk(sink).await?;
                }
            }
            Some(rendezvous_message::Union::RegisterPkResponse(rpr)) => {
                update_latency();
                match rpr.result.enum_value() {
                    Ok(register_pk_response::Result::OK) => {
                        Config::set_key_confirmed(true);
                        Config::set_host_key_confirmed(&self.host_prefix, true);
                        *SOLVING_PK_MISMATCH.lock().await = "".to_owned();
                        NEEDS_DEPLOY.store(false, Ordering::SeqCst);
                        // 定制：握手已完成，停止补发 RegisterPk。
                        self.pk_retry = 0;
                        #[cfg(target_os = "android")]
                        reset_needs_deploy_notification();
                    }
                    Ok(register_pk_response::Result::UUID_MISMATCH) => {
                        self.handle_uuid_mismatch(sink).await?;
                    }
                    Ok(register_pk_response::Result::NOT_DEPLOYED) => {
                        if !NEEDS_DEPLOY.load(Ordering::SeqCst) {
                            log::warn!("Server requires deployment. Run `rustdesk --deploy --token <api_token>` on this device.");
                        }
                        NEEDS_DEPLOY.store(true, Ordering::SeqCst);
                        // Clear key_confirmed so the UI reflects the truth: this device is
                        // not currently registered. Covers the case where an online device
                        // was deleted by an admin while running.
                        Config::set_key_confirmed(false);
                        Config::set_host_key_confirmed(&self.host_prefix, false);
                        #[cfg(target_os = "android")]
                        notify_android_needs_deploy();
                    }
                    _ => {
                        log::error!("unknown RegisterPkResponse");
                    }
                }
                if rpr.keep_alive > 0 {
                    self.keep_alive = rpr.keep_alive * 1000;
                    log::info!("keep_alive: {}ms", self.keep_alive);
                }
            }
            Some(rendezvous_message::Union::PunchHole(ph)) => {
                let rz = self.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(rz.handle_punch_hole(ph, server).await);
                });
            }
            Some(rendezvous_message::Union::RequestRelay(rr)) => {
                let rz = self.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(rz.handle_request_relay(rr, server).await);
                });
            }
            Some(rendezvous_message::Union::FetchLocalAddr(fla)) => {
                let rz = self.clone();
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(rz.handle_intranet(fla, server).await);
                });
            }
            Some(rendezvous_message::Union::ConfigureUpdate(cu)) => {
                let v0 = Config::get_rendezvous_servers();
                Config::set_option(
                    "rendezvous-servers".to_owned(),
                    cu.rendezvous_servers.join(","),
                );
                Config::set_serial(cu.serial);
                if v0 != Config::get_rendezvous_servers() {
                    Self::restart();
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub async fn start_tcp(server: ServerPtr, host: String) -> ResultType<()> {
        let host = check_port(&host, RENDEZVOUS_PORT);
        log::info!("start tcp: {}", hbb_common::websocket::check_ws(&host));
        // 定制：同 start_udp，按可达性择优（IPv4/IPv6 双栈环境下避免选到不通的一侧）。
        let target = crate::net_target::pick_reachable(&host).await;
        let mut conn = connect_tcp(target, CONNECT_TIMEOUT).await?;
        let key = crate::get_key(true).await;
        crate::secure_tcp(&mut conn, &key).await?;
        let mut rz = Self {
            addr: conn.local_addr().into_target_addr()?,
            host: host.clone(),
            host_prefix: Self::get_host_prefix(&host),
            keep_alive: crate::DEFAULT_KEEP_ALIVE,
            pk_retry: 0,
        };
        let mut timer = crate::rustdesk_interval(interval(crate::TIMER_OUT));
        let mut last_register_sent: Option<Instant> = None;
        let mut last_recv_msg = Instant::now();
        // we won't support connecting to multiple rendzvous servers any more, so we can use a global variable here.
        Config::set_host_key_confirmed(&rz.host_prefix, false);
        loop {
            let mut update_latency = || {
                let latency = last_register_sent
                    .map(|x| x.elapsed().as_micros() as i64)
                    .unwrap_or(0);
                Config::update_latency(&host, latency);
                log::debug!("Latency of {}: {}ms", host, latency as f64 / 1000.);
            };
            select! {
                res = conn.next() => {
                    last_recv_msg = Instant::now();
                    let bytes = res.ok_or_else(|| anyhow::anyhow!("Rendezvous connection is reset by the peer"))??;
                    if bytes.is_empty() {
                        // After fixing frequent register_pk, for websocket, nginx need to set proxy_read_timeout to more than 60 seconds, eg: 120s
                        // https://serverfault.com/questions/1060525/why-is-my-websocket-connection-gets-closed-in-60-seconds
                        conn.send_bytes(bytes::Bytes::new()).await?;
                        continue; // heartbeat
                    }
                    let msg = Message::parse_from_bytes(&bytes)?;
                    rz.handle_resp(msg.union, Sink::Stream(&mut conn), &server, &mut update_latency).await?
                }
                _ = timer.tick() => {
                    if SHOULD_EXIT.load(Ordering::SeqCst) {
                        break;
                    }
                    // https://www.emqx.com/en/blog/mqtt-keep-alive
                    if last_recv_msg.elapsed().as_millis() as u64 > rz.keep_alive as u64 * 3 / 2 {
                        bail!("Rendezvous connection is timeout");
                    }
                    if (!Config::get_key_confirmed() ||
                        !Config::get_host_key_confirmed(&rz.host_prefix)) &&
                        last_register_sent.map(|x| x.elapsed().as_millis() as i64).unwrap_or(REG_INTERVAL) >= REG_INTERVAL {
                        rz.register_pk(Sink::Stream(&mut conn)).await?;
                        last_register_sent = Some(Instant::now());
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn start(server: ServerPtr, host: String) -> ResultType<()> {
        log::info!("start rendezvous mediator of {}", host);
        //If the investment agent type is http or https, then tcp forwarding is enabled.
        if (cfg!(debug_assertions) && option_env!("TEST_TCP").is_some())
            || Config::is_proxy()
            || use_ws()
            || crate::is_udp_disabled()
            // 定制：UDP 通道已证实不通（本进程内连续失败到阈值），保持走 TCP 注册，
            // 保证被控端始终可被连接。重启 app 复位回 UDP 优先。
            || FORCE_TCP_AFTER_UDP_FAIL.load(Ordering::SeqCst)
        {
            return Self::start_tcp(server, host).await;
        }
        match Self::start_udp(server, host).await {
            Err(err) if err.to_string().contains(ERR_UDP_FALLBACK) => {
                log::warn!("{err}; switching this run to tcp registration");
                FORCE_TCP_AFTER_UDP_FAIL.store(true, Ordering::SeqCst);
                Err(err)
            }
            other => other,
        }
    }

    async fn handle_request_relay(&self, rr: RequestRelay, server: ServerPtr) -> ResultType<()> {
        let addr = AddrMangle::decode(&rr.socket_addr);
        let last = *LAST_RELAY_MSG.lock().await;
        *LAST_RELAY_MSG.lock().await = (addr, Instant::now());
        // skip duplicate relay request messages
        if last.0 == addr && last.1.elapsed().as_millis() < 100 {
            return Ok(());
        }
        let meta = connection_meta(
            rr.control_permissions.into_option(),
            rr.controlled_context.into_option(),
        );

        self.create_relay(
            rr.socket_addr.into(),
            rr.relay_server,
            rr.uuid,
            server,
            rr.secure,
            false,
            Default::default(),
            meta,
        )
        .await
    }

    async fn create_relay(
        &self,
        socket_addr: Vec<u8>,
        relay_server: String,
        uuid: String,
        server: ServerPtr,
        secure: bool,
        initiate: bool,
        socket_addr_v6: bytes::Bytes,
        meta: ConnectionMeta,
    ) -> ResultType<()> {
        let peer_addr = AddrMangle::decode(&socket_addr);
        log::info!(
            "create_relay requested from {:?}, relay_server: {}, uuid: {}, secure: {}",
            peer_addr,
            relay_server,
            uuid,
            secure,
        );

        let mut socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;

        let mut msg_out = Message::new();
        let mut rr = RelayResponse {
            socket_addr: socket_addr.into(),
            version: crate::VERSION.to_owned(),
            socket_addr_v6,
            ..Default::default()
        };
        if initiate {
            rr.uuid = uuid.clone();
            rr.relay_server = relay_server.clone();
            rr.set_id(Config::get_id());
        }
        msg_out.set_relay_response(rr);
        socket.send(&msg_out).await?;
        crate::create_relay_connection(
            server,
            relay_server,
            uuid,
            peer_addr,
            secure,
            is_ipv4(&self.addr),
            meta,
        )
        .await;
        Ok(())
    }

    async fn handle_intranet(&self, fla: FetchLocalAddr, server: ServerPtr) -> ResultType<()> {
        let addr = AddrMangle::decode(&fla.socket_addr);
        let last = *LAST_MSG.lock().await;
        *LAST_MSG.lock().await = (addr, Instant::now());
        // skip duplicate punch hole messages
        if last.0 == addr && last.1.elapsed().as_millis() < 100 {
            return Ok(());
        }
        let peer_addr_v6 = hbb_common::AddrMangle::decode(&fla.socket_addr_v6);
        let relay_server = self.get_relay_server(fla.relay_server.clone());
        let relay = use_ws() || Config::is_proxy();
        let mut socket_addr_v6 = Default::default();
        let meta = connection_meta(
            fla.control_permissions.clone().into_option(),
            fla.controlled_context.clone().into_option(),
        );
        if peer_addr_v6.port() > 0 && !relay {
            socket_addr_v6 = start_ipv6(peer_addr_v6, addr, server.clone(), meta.clone()).await;
        }
        if is_ipv4(&self.addr) && !relay && !config::is_disable_tcp_listen() {
            if let Err(err) = self
                .handle_intranet_(
                    fla.clone(),
                    server.clone(),
                    relay_server.clone(),
                    socket_addr_v6.clone(),
                    meta.clone(),
                )
                .await
            {
                log::debug!("Failed to handle intranet: {:?}, will try relay", err);
            } else {
                return Ok(());
            }
        }
        let uuid = Uuid::new_v4().to_string();
        self.create_relay(
            fla.socket_addr.into(),
            relay_server,
            uuid,
            server,
            true,
            true,
            socket_addr_v6,
            meta,
        )
        .await
    }

    async fn handle_intranet_(
        &self,
        fla: FetchLocalAddr,
        server: ServerPtr,
        relay_server: String,
        socket_addr_v6: bytes::Bytes,
        meta: ConnectionMeta,
    ) -> ResultType<()> {
        let peer_addr = AddrMangle::decode(&fla.socket_addr);
        log::debug!("Handle intranet from {:?}", peer_addr);
        let mut socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;
        let local_addr = socket.local_addr();
        // we saw invalid local_addr while using proxy, local_addr.ip() == "::1"
        let local_addr: SocketAddr =
            format!("{}:{}", local_addr.ip(), local_addr.port()).parse()?;
        let mut msg_out = Message::new();
        msg_out.set_local_addr(LocalAddr {
            id: Config::get_id(),
            socket_addr: AddrMangle::encode(peer_addr).into(),
            local_addr: AddrMangle::encode(local_addr).into(),
            relay_server,
            version: crate::VERSION.to_owned(),
            socket_addr_v6,
            ..Default::default()
        });
        let bytes = msg_out.write_to_bytes()?;
        socket.send_raw(bytes).await?;
        crate::accept_connection(server.clone(), socket, peer_addr, true, meta).await;
        Ok(())
    }

    async fn handle_punch_hole(&self, ph: PunchHole, server: ServerPtr) -> ResultType<()> {
        let mut peer_addr = AddrMangle::decode(&ph.socket_addr);
        let last = *LAST_MSG.lock().await;
        *LAST_MSG.lock().await = (peer_addr, Instant::now());
        // skip duplicate punch hole messages
        if last.0 == peer_addr && last.1.elapsed().as_millis() < 100 {
            return Ok(());
        }
        let peer_addr_v6 = hbb_common::AddrMangle::decode(&ph.socket_addr_v6);
        let relay = use_ws() || Config::is_proxy() || ph.force_relay;
        let mut socket_addr_v6 = Default::default();
        let meta = connection_meta(
            ph.control_permissions.into_option(),
            ph.controlled_context.into_option(),
        );
        if peer_addr_v6.port() > 0 && !relay {
            socket_addr_v6 =
                start_ipv6(peer_addr_v6, peer_addr, server.clone(), meta.clone()).await;
        }
        let relay_server = self.get_relay_server(ph.relay_server);
        // for ensure, websocket go relay directly
        if ph.nat_type.enum_value() == Ok(NatType::SYMMETRIC)
            || Config::get_nat_type() == NatType::SYMMETRIC as i32
            || relay
            || (config::is_disable_tcp_listen() && ph.udp_port <= 0)
        {
            let uuid = Uuid::new_v4().to_string();
            return self
                .create_relay(
                    ph.socket_addr.into(),
                    relay_server,
                    uuid,
                    server,
                    true,
                    true,
                    socket_addr_v6.clone(),
                    meta,
                )
                .await;
        }
        use hbb_common::protobuf::Enum;
        let nat_type = NatType::from_i32(Config::get_nat_type()).unwrap_or(NatType::UNKNOWN_NAT);
        let msg_punch = PunchHoleSent {
            socket_addr: ph.socket_addr,
            id: Config::get_id(),
            relay_server,
            nat_type: nat_type.into(),
            version: crate::VERSION.to_owned(),
            socket_addr_v6,
            ..Default::default()
        };
        if ph.udp_port > 0 {
            peer_addr.set_port(ph.udp_port as u16);
            self.punch_udp_hole(peer_addr, server, msg_punch, meta)
                .await?;
            return Ok(());
        }
        log::debug!("Punch tcp hole to {:?}", peer_addr);
        let mut socket = {
            let socket = connect_tcp(&*self.host, CONNECT_TIMEOUT).await?;
            let local_addr = socket.local_addr();
            // key important here for punch hole to tell my gateway incoming peer is safe.
            // it can not be async here, because local_addr can not be reused, we must close the connection before use it again.
            allow_err!(socket_client::connect_tcp_local(peer_addr, Some(local_addr), 30).await);
            socket
        };
        let mut msg_out = Message::new();
        msg_out.set_punch_hole_sent(msg_punch);
        let bytes = msg_out.write_to_bytes()?;
        socket.send_raw(bytes).await?;
        crate::accept_connection(server.clone(), socket, peer_addr, true, meta).await;
        Ok(())
    }

    async fn punch_udp_hole(
        &self,
        peer_addr: SocketAddr,
        server: ServerPtr,
        msg_punch: PunchHoleSent,
        meta: ConnectionMeta,
    ) -> ResultType<()> {
        let mut msg_out = Message::new();
        msg_out.set_punch_hole_sent(msg_punch);
        let (socket, addr) = new_direct_udp_for(&self.host).await?;
        let data = msg_out.write_to_bytes()?;
        socket.send_to(&data, addr).await?;
        let socket_cloned = socket.clone();
        tokio::spawn(async move {
            for _ in 0..2 {
                let tm = (hbb_common::time_based_rand() % 20 + 10) as f32 / 1000.;
                hbb_common::sleep(tm).await;
                socket.send_to(&data, addr).await.ok();
            }
        });
        udp_nat_listen(socket_cloned.clone(), peer_addr, peer_addr, server, meta).await?;
        Ok(())
    }

    async fn register_pk(&mut self, socket: Sink<'_>) -> ResultType<()> {
        // Throttle register_pk when the device is awaiting deployment: server
        // already told us we're not in its db; sending more often than every
        // DEPLOY_RETRY_INTERVAL ms is wasted traffic until the operator runs
        // `rustdesk --deploy --token <api_token>`.
        if NEEDS_DEPLOY.load(Ordering::SeqCst) {
            let mut last = LAST_NOT_DEPLOYED_REGISTER.lock().await;
            if let Some(t) = *last {
                if (t.elapsed().as_millis() as i64) < DEPLOY_RETRY_INTERVAL {
                    return Ok(());
                }
            }
            *last = Some(Instant::now());
        } else {
            *LAST_NOT_DEPLOYED_REGISTER.lock().await = None;
        }
        let mut msg_out = Message::new();
        let pk = Config::get_key_pair().1;
        let uuid = hbb_common::get_uuid();
        let id = Config::get_id();
        msg_out.set_register_pk(RegisterPk {
            id,
            uuid: uuid.into(),
            pk: pk.into(),
            no_register_device: Config::no_register_device(),
            ..Default::default()
        });
        socket.send(&msg_out).await?;
        SENT_REGISTER_PK.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn handle_uuid_mismatch(&mut self, socket: Sink<'_>) -> ResultType<()> {
        {
            let mut solving = SOLVING_PK_MISMATCH.lock().await;
            if solving.is_empty() || *solving == self.host {
                log::info!("UUID_MISMATCH received from {}", self.host);
                Config::set_key_confirmed(false);
                Config::update_id();
                *solving = self.host.clone();
            } else {
                return Ok(());
            }
        }
        self.register_pk(socket).await
    }

    async fn register_peer(&mut self, socket: Sink<'_>) -> ResultType<()> {
        let solving = SOLVING_PK_MISMATCH.lock().await;
        if !(solving.is_empty() || *solving == self.host) {
            return Ok(());
        }
        drop(solving);
        if !Config::get_key_confirmed() || !Config::get_host_key_confirmed(&self.host_prefix) {
            log::info!(
                "register_pk of {} due to key not confirmed",
                self.host_prefix
            );
            return self.register_pk(socket).await;
        }
        let id = Config::get_id();
        log::trace!(
            "Register my id {:?} to rendezvous server {:?}",
            id,
            self.addr,
        );
        let mut msg_out = Message::new();
        let serial = Config::get_serial();
        msg_out.set_register_peer(RegisterPeer {
            id,
            serial,
            ..Default::default()
        });
        socket.send(&msg_out).await?;
        Ok(())
    }

    fn get_relay_server(&self, provided_by_rendezvous_server: String) -> String {
        let mut relay_server = Config::get_option("relay-server");
        if relay_server.is_empty() {
            relay_server = provided_by_rendezvous_server;
        }
        if relay_server.is_empty() {
            relay_server = crate::increase_port(&self.host, 1);
        }
        relay_server
    }
}

fn get_direct_port() -> i32 {
    let mut port = Config::get_option("direct-access-port")
        .parse::<i32>()
        .unwrap_or(0);
    if port <= 0 {
        port = RENDEZVOUS_PORT + 2;
    }
    port
}

async fn direct_server(server: ServerPtr) {
    let mut listener = None;
    let mut port = 0;
    loop {
        let disabled = !option2bool(
            OPTION_DIRECT_SERVER,
            &Config::get_option(OPTION_DIRECT_SERVER),
        ) || option2bool("stop-service", &Config::get_option("stop-service"));
        if !disabled && listener.is_none() {
            port = get_direct_port();
            match hbb_common::tcp::listen_any(port as _).await {
                Ok(l) => {
                    listener = Some(l);
                    log::info!(
                        "Direct server listening on: {:?}",
                        listener.as_ref().map(|l| l.local_addr())
                    );
                }
                Err(err) => {
                    // to-do: pass to ui
                    log::error!(
                        "Failed to start direct server on port: {}, error: {}",
                        port,
                        err
                    );
                    loop {
                        if port != get_direct_port() {
                            break;
                        }
                        sleep(1.).await;
                    }
                }
            }
        }
        if let Some(l) = listener.as_mut() {
            if disabled || port != get_direct_port() {
                log::info!("Exit direct access listen");
                listener = None;
                continue;
            }
            if let Ok(Ok((stream, addr))) = hbb_common::timeout(1000, l.accept()).await {
                stream.set_nodelay(true).ok();
                log::info!("direct access from {}", addr);
                let local_addr = stream
                    .local_addr()
                    .unwrap_or(Config::get_any_listen_addr(true));
                let server = server.clone();
                tokio::spawn(async move {
                    allow_err!(
                        crate::server::create_tcp_connection(
                            server,
                            hbb_common::Stream::from(stream, local_addr),
                            addr,
                            false,
                            ConnectionMeta::default(), // Direct connections don't have server-side user context.
                        )
                        .await
                    );
                });
            } else {
                sleep(0.1).await;
            }
        } else {
            sleep(1.).await;
        }
    }
}

enum Sink<'a> {
    Framed(&'a mut FramedSocket, &'a TargetAddr<'a>),
    Stream(&'a mut Stream),
}

impl Sink<'_> {
    async fn send(self, msg: &Message) -> ResultType<()> {
        match self {
            Sink::Framed(socket, addr) => socket.send(msg, addr.to_owned()).await,
            Sink::Stream(stream) => stream.send(msg).await,
        }
    }
}

async fn start_ipv6(
    peer_addr_v6: SocketAddr,
    peer_addr_v4: SocketAddr,
    server: ServerPtr,
    meta: ConnectionMeta,
) -> bytes::Bytes {
    crate::test_ipv6().await;
    if let Some((socket, local_addr_v6)) = crate::get_ipv6_socket().await {
        let server = server.clone();
        tokio::spawn(async move {
            allow_err!(
                udp_nat_listen(socket.clone(), peer_addr_v6, peer_addr_v4, server, meta).await
            );
        });
        return local_addr_v6;
    }
    Default::default()
}

async fn udp_nat_listen(
    socket: Arc<tokio::net::UdpSocket>,
    peer_addr: SocketAddr,
    peer_addr_v4: SocketAddr,
    server: ServerPtr,
    meta: ConnectionMeta,
) -> ResultType<()> {
    let tm = Instant::now();
    let socket_cloned = socket.clone();
    let func = async {
        socket.connect(peer_addr).await?;
        let res = crate::punch_udp(socket.clone(), true).await?;
        let stream = crate::kcp_stream::KcpStream::accept(
            socket,
            Duration::from_millis(CONNECT_TIMEOUT as _),
            res,
        )
        .await?;
        crate::server::create_tcp_connection(server, stream.1, peer_addr_v4, true, meta).await?;
        Ok(())
    };
    func.await.map_err(|e: anyhow::Error| {
        anyhow::anyhow!(
            "Stop listening on {:?} for remote {peer_addr} with KCP, {:?} elapsed: {e}",
            socket_cloned.local_addr(),
            tm.elapsed()
        )
    })?;
    Ok(())
}

// When config is not yet synced from root, register_pk may have already been sent with a new generated pk.
// After config sync completes, the pk may change. This struct detects pk changes and triggers
// a re-registration by setting key_confirmed to false.
// NOTE:
// This only corrects PK registration for the current ID. If root uses a non-default mac-generated ID,
// this does not resolve the multi-ID issue by itself.
pub struct CheckIfResendPk {
    pk: Option<Vec<u8>>,
}
impl CheckIfResendPk {
    pub fn new() -> Self {
        Self {
            pk: Config::get_cached_pk(),
        }
    }
}
impl Drop for CheckIfResendPk {
    fn drop(&mut self) {
        if SENT_REGISTER_PK.load(Ordering::SeqCst) && Config::get_cached_pk() != self.pk {
            Config::set_key_confirmed(false);
            log::info!("Set key_confirmed to false due to pk changed, will resend register_pk");
        }
    }
}
