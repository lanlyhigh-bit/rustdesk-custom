//! 定制（2026-10-23）：域名解析结果择优。
//!
//! 上游 `hbb_common::socket_client::test_target` 在 TCP 探测失败后只取
//! `lookup_host(target).next()`，即 DNS 返回的**第一个**地址——既不校验可达性，
//! 也没有 fallback。当同一个域名同时有 A（IPv4）与 AAAA（IPv6）、而客户端
//! 所在网络只有其中一种协议真正可用时，选错就表现为「解析成功但一直未就绪 /
//! 连不上」，且 60s 后的 rebind 仍按同样顺序再选一次。
//!
//! 这里在主仓库侧加一层并发探测：所有解析结果同时试 TCP，取第一个可达的；
//! 只有一个候选、本身是 IP 字面量、或全部不可达时原样返回，行为与改造前一致
//! （不新增失败模式，也不改变 relay/打洞的协议语义）。

use std::{net::SocketAddr, time::Duration};

use hbb_common::{futures::future::join_all, log, tokio};

/// 单地址 TCP 探测上限。并发执行，不叠加。
const PROBE_TIMEOUT_MS: u64 = 300;

/// 把 `host[:port]` 变成当前网络下可达的 `ip:port`。
///
/// 返回值为字符串，直接喂给 `connect_tcp` / `new_udp_for` 等上游函数；
/// 因为这些函数对 IP 字面量不再做 DNS 解析，等价于把选择权从系统解析顺序
/// 收回到「谁先连得通」。
pub async fn pick_reachable(target: &str) -> String {
    if is_ip_literal(target) {
        return target.to_owned();
    }
    let addrs: Vec<SocketAddr> = match tokio::net::lookup_host(target).await {
        Ok(it) => it.collect(),
        Err(err) => {
            log::debug!("pick_reachable: lookup {target} failed: {err}");
            return target.to_owned();
        }
    };
    if addrs.len() <= 1 {
        return target.to_owned();
    }
    let probes = addrs.iter().enumerate().map(|(idx, addr)| {
        let addr = *addr;
        async move {
            let ok = tokio::time::timeout(
                Duration::from_millis(PROBE_TIMEOUT_MS),
                tokio::net::TcpStream::connect(addr),
            )
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false);
            (idx, addr, ok)
        }
    });
    let mut best: Option<(usize, SocketAddr)> = None;
    for (idx, addr, ok) in join_all(probes).await {
        if ok && best.map(|(i, _)| idx < i).unwrap_or(true) {
            best = Some((idx, addr));
        }
    }
    match best {
        Some((idx, addr)) => {
            log::info!(
                "pick_reachable: {target} -> {addr} (candidate {} of {})",
                idx + 1,
                addrs.len()
            );
            addr.to_string()
        }
        None => {
            log::debug!(
                "pick_reachable: no reachable candidate among {} for {target}, keep as is",
                addrs.len()
            );
            target.to_owned()
        }
    }
}

fn is_ip_literal(target: &str) -> bool {
    if target.parse::<SocketAddr>().is_ok() {
        return true;
    }
    target
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok()
}
