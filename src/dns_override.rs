//! 上游域名解析三级兜底（2026-10-24，OpenClash 启动窗口期 DNS 断供事故）：
//!   L1 自定义 DNS 服务器（UDP 直查，绕开路由器 dnsmasq/OpenClash 劫持链）
//!   L2 静态 IP 映射（配置内指定，DNS 全挂时的最后手段）
//!   L3 系统 getaddrinfo（既有行为兜底）
//!
//! 配置：`config/dns-overrides.json`（路径 env `LLM_PROVIDER_ROUTER_DNS_CONFIG_PATH`）。
//! 文件缺失 / `enabled=false` / 解析失败 = 功能关闭，reqwest 回到系统解析，无行为变化。
//!
//! DNS wire 格式为最小手写实现（仅 A/AAAA 查询与应答解析，含压缩指针跳名），
//! 零新增依赖；不覆盖 CNAME 链路（对供应商 API 域名足够，A 记录直接可用）。

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsOverrideFile {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// L1 自定义解析服务器（"223.5.5.5" 或 "223.5.5.5:53"），按序尝试，首个非空应答生效。
    #[serde(default)]
    pub nameservers: Vec<String>,
    /// 单次查询超时（毫秒）。
    #[serde(default = "default_query_timeout_ms")]
    pub query_timeout_ms: u64,
    /// 解析结果缓存秒数（进程内，避免连接抖动打爆上游 DNS）。
    #[serde(default = "default_cache_ttl_seconds")]
    pub cache_ttl_seconds: u64,
    /// L2 静态映射：域名 -> IP 列表。仅当 L1 未命中时使用；L3 在两者皆空后兜底。
    #[serde(default, rename = "static")]
    pub static_map: HashMap<String, Vec<String>>,
}

fn default_true() -> bool {
    true
}
fn default_query_timeout_ms() -> u64 {
    1500
}
fn default_cache_ttl_seconds() -> u64 {
    60
}

impl DnsOverrideFile {
    /// 读取配置文件；缺失/损坏/enabled=false 一律返回 None（功能关闭）。
    pub fn load(path: &str) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        let cfg: Self = match serde_json::from_str(&raw) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("[dns-override] 配置解析失败 {path}: {e}，DNS 兜底关闭");
                return None;
            }
        };
        if !cfg.enabled {
            return None;
        }
        Some(cfg)
    }
}

/// 写盘前结构校验：DNS 服务器地址、静态 IP、超时合法性。
pub fn validate(cfg: &DnsOverrideFile) -> Result<(), String> {
    for s in &cfg.nameservers {
        let owned = if s.contains(':') {
            s.to_string()
        } else {
            format!("{s}:53")
        };
        owned
            .parse::<SocketAddr>()
            .map_err(|_| format!("非法 DNS 服务器地址: {s}"))?;
    }
    for (host, ips) in &cfg.static_map {
        if host.trim().is_empty() {
            return Err("静态映射域名为空".into());
        }
        for ip in ips {
            ip.parse::<IpAddr>()
                .map_err(|_| format!("静态映射 {host} 含非法 IP: {ip}"))?;
        }
    }
    if cfg.query_timeout_ms == 0 {
        return Err("query_timeout_ms 必须大于 0".into());
    }
    if cfg.cache_ttl_seconds > 3600 {
        return Err("cache_ttl_seconds 过大（上限 3600）".into());
    }
    Ok(())
}

fn parse_resolver_config(cfg: &DnsOverrideFile) -> Option<ResolverConfig> {
    let nameservers: Vec<SocketAddr> = cfg
        .nameservers
        .iter()
        .filter_map(|s| {
            let owned = if s.contains(':') {
                s.to_string()
            } else {
                format!("{s}:53")
            };
            owned.parse().ok()
        })
        .collect();
    let mut static_map: HashMap<String, Vec<IpAddr>> = HashMap::new();
    for (host, ips) in &cfg.static_map {
        let parsed: Vec<IpAddr> = ips.iter().filter_map(|ip| ip.parse().ok()).collect();
        if !parsed.is_empty() {
            static_map.insert(host.trim_end_matches('.').to_ascii_lowercase(), parsed);
        }
    }
    if nameservers.is_empty() && static_map.is_empty() {
        return None;
    }
    Some(ResolverConfig {
        nameservers,
        static_map: Arc::new(static_map),
        query_timeout: Duration::from_millis(cfg.query_timeout_ms),
    })
}

/// 从配置文件直接构建解析器（含启动日志）；功能关闭/配置无效时返回 None。
pub fn resolver_from_config_file(path: &str) -> Option<DnsOverrideResolver> {
    let cfg = DnsOverrideFile::load(path)?;
    DnsOverrideResolver::from_config(&cfg).inspect(|_| {
        eprintln!(
            "[dns-override] 已启用：L1 自定义 DNS {:?}；L2 静态映射 {} 条；查询超时 {:?}",
            cfg.nameservers,
            cfg.static_map.len(),
            Duration::from_millis(cfg.query_timeout_ms)
        );
    })
}

type StaticMap = Arc<HashMap<String, Vec<IpAddr>>>;
type CacheMap = Arc<Mutex<HashMap<String, (Instant, Vec<IpAddr>)>>>;

/// 解析器可热更配置快照（dashboard 保存后整体替换）。
#[derive(Clone)]
struct ResolverConfig {
    nameservers: Vec<SocketAddr>,
    static_map: StaticMap,
    query_timeout: Duration,
}

#[derive(Clone)]
pub struct DnsOverrideResolver {
    config: Arc<RwLock<ResolverConfig>>,
    cache_ttl: Duration,
    cache: CacheMap,
}

impl DnsOverrideResolver {
    /// 构建三级兜底解析器。任一级配置非法时降级（跳过该级），全空则返回 None。
    pub fn from_config(cfg: &DnsOverrideFile) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }
        let config = parse_resolver_config(cfg)?;
        Some(Self {
            config: Arc::new(RwLock::new(config)),
            cache_ttl: Duration::from_secs(cfg.cache_ttl_seconds),
            cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// dashboard 保存后热更新：整体替换配置快照并清空旧缓存（旧 IP 可能已失效）。
    pub fn update_config(&self, cfg: &DnsOverrideFile) -> Result<(), String> {
        let config =
            parse_resolver_config(cfg).ok_or("至少需要一台 DNS 服务器或一条静态 IP 映射")?;
        *self.config.write().expect("dns config lock") = config;
        self.cache.lock().unwrap().clear();
        Ok(())
    }

    /// 三级解析链。任一级命中即返回；全部失败才报错（reqwest 侧按连接失败走既有重试/熔断）。
    async fn resolve_chain(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.parse::<IpAddr>().is_ok() {
            // URL 里本来就是 IP（配置直写场景），无需解析。
            return Ok(vec![host.parse().expect("checked")]);
        }
        if let Some((at, addrs)) = self.cache.lock().unwrap().get(&host) {
            if at.elapsed() < self.cache_ttl {
                return Ok(addrs.clone());
            }
        }
        let cfg = self.config.read().expect("dns config lock").clone();

        // L1：自定义 DNS 服务器
        if !cfg.nameservers.is_empty() {
            for server in &cfg.nameservers {
                match self.query_server(*server, &host, cfg.query_timeout).await {
                    Ok(addrs) if !addrs.is_empty() => {
                        self.cache_store(&host, &addrs);
                        return Ok(addrs);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("[dns-override] L1 查询 {host} @{server} 失败: {e}，尝试下一级");
                    }
                }
            }
        }

        // L2：静态映射
        if let Some(addrs) = cfg.static_map.get(&host) {
            self.cache_store(&host, addrs);
            return Ok(addrs.clone());
        }

        // L3：系统解析（glibc /etc/resolv.conf 链路；线程 + recv_timeout 防悬挂拖死请求，
        // 超时后线程留在后台自然退出——getaddrinfo 无超时控制，泄漏至多一个线程可接受）
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn({
            let host = host.clone();
            move || {
                let r = (host.as_str(), 0)
                    .to_socket_addrs()
                    .map(|it| it.map(|sa| sa.ip()).collect::<Vec<_>>());
                let _ = tx.send(r);
            }
        });
        match rx.recv_timeout(cfg.query_timeout * 4) {
            Ok(Ok(addrs)) if !addrs.is_empty() => {
                self.cache_store(&host, &addrs);
                Ok(addrs)
            }
            Ok(Err(e)) => Err(format!("L3 系统解析 {host}: {e}")),
            _ => Err(format!("L3 系统解析 {host} 超时或无结果")),
        }
    }

    fn cache_store(&self, host: &str, addrs: &[IpAddr]) {
        self.cache
            .lock()
            .unwrap()
            .insert(host.to_string(), (Instant::now(), addrs.to_vec()));
    }

    /// 对单个服务器并发发 A + AAAA 查询，合并去重；单类型失败不拖垮另一类型。
    async fn query_server(
        &self,
        server: SocketAddr,
        host: &str,
        query_timeout: Duration,
    ) -> Result<Vec<IpAddr>, String> {
        let (a, aaaa) = tokio::join!(
            self.query_single(server, host, RecordType::A, query_timeout),
            self.query_single(server, host, RecordType::AAAA, query_timeout)
        );
        let mut out: Vec<IpAddr> = Vec::new();
        let mut first_err: Option<String> = None;
        for part in [a, aaaa] {
            match part {
                Ok(addrs) => out.extend(addrs),
                Err(e) if first_err.is_none() => first_err = Some(e),
                Err(_) => {}
            }
        }
        if out.is_empty() {
            return Err(first_err.unwrap_or_else(|| "无应答记录".into()));
        }
        Ok(out)
    }

    async fn query_single(
        &self,
        server: SocketAddr,
        host: &str,
        rtype: RecordType,
        query_timeout: Duration,
    ) -> Result<Vec<IpAddr>, String> {
        let packet = build_query(host, rtype)?;
        let bind_addr: SocketAddr = if server.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid bind addr")
        } else {
            "[::]:0".parse().expect("valid bind addr")
        };
        let sock = tokio::net::UdpSocket::bind(bind_addr)
            .await
            .map_err(|e| format!("bind: {e}"))?;
        sock.connect(server)
            .await
            .map_err(|e| format!("connect {server}: {e}"))?;
        sock.send(&packet).await.map_err(|e| format!("send: {e}"))?;

        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(query_timeout, sock.recv(&mut buf))
            .await
            .map_err(|_| format!("timeout after {query_timeout:?}"))?
            .map_err(|e| format!("recv: {e}"))?;
        parse_answer_addrs(&buf[..n], rtype)
    }
}

impl Resolve for DnsOverrideResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let this = self.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = this
                .resolve_chain(&host)
                .await
                .map_err(std::io::Error::other)?;
            // 端口 0：reqwest 会按 URL scheme 覆盖为常规端口（与内置 GaiResolver 一致）。
            let addrs: Addrs = Box::new(addrs.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

// ---------------------------------------------------------------------------
// 最小 DNS wire 协议（RFC 1035）：仅构造 A/AAAA 查询与解析应答中的 A/AAAA 记录。
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordType(pub u16);

impl RecordType {
    pub const A: RecordType = RecordType(1);
    pub const AAAA: RecordType = RecordType(28);
}

/// 构造标准递归查询报文。
pub fn build_query(host: &str, rtype: RecordType) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(18 + host.len());
    let id: u16 = rand::random();
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&[0x01, 0x00]); // flags: RD=1
    out.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // qdcount=1，其余 0
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!("非法域名标签: {label:?}"));
        }
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0); // QNAME 结束
    out.extend_from_slice(&rtype.0.to_be_bytes());
    out.extend_from_slice(&[0, 1]); // QCLASS=IN
    Ok(out)
}

/// 解析应答中的 A/AAAA 记录（跳过问题段；答案名支持压缩指针）。
/// 域名归属校验从宽：CNAME 链场景下 answer name 可能是链中段，以 rtype 匹配为准。
pub fn parse_answer_addrs(packet: &[u8], rtype: RecordType) -> Result<Vec<IpAddr>, String> {
    if packet.len() < 12 {
        return Err("报文过短".into());
    }
    let rcode = packet[3] & 0x0f;
    if rcode != 0 {
        return Err(format!("DNS rcode={rcode}"));
    }
    let qdcount = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let ancount = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    let mut pos = 12;
    for _ in 0..qdcount {
        pos = skip_name(packet, pos)?;
        pos = pos.checked_add(4).ok_or("问题段越界")?;
    }
    let mut out = Vec::new();
    for _ in 0..ancount {
        pos = skip_name(packet, pos)?;
        if pos + 10 > packet.len() {
            return Err("答案段越界".into());
        }
        let atype = u16::from_be_bytes([packet[pos], packet[pos + 1]]);
        let rdlength = u16::from_be_bytes([packet[pos + 8], packet[pos + 9]]) as usize;
        let rdata = pos + 10;
        if rdata + rdlength > packet.len() {
            return Err("RDATA 越界".into());
        }
        if atype == rtype.0 {
            match rtype.0 {
                1 if rdlength == 4 => {
                    out.push(IpAddr::V4(std::net::Ipv4Addr::new(
                        packet[rdata],
                        packet[rdata + 1],
                        packet[rdata + 2],
                        packet[rdata + 3],
                    )));
                }
                28 if rdlength == 16 => {
                    let mut o = [0u8; 16];
                    o.copy_from_slice(&packet[rdata..rdata + 16]);
                    out.push(IpAddr::V6(std::net::Ipv6Addr::from(o)));
                }
                _ => {}
            }
        }
        pos = rdata + rdlength;
    }
    Ok(out)
}

/// 跳过一个（可能含压缩指针的）域名，返回下一位置。
fn skip_name(packet: &[u8], mut pos: usize) -> Result<usize, String> {
    loop {
        let Some(&len) = packet.get(pos) else {
            return Err("域名解析越界".into());
        };
        if len & 0xc0 == 0xc0 {
            return pos.checked_add(2).ok_or_else(|| "压缩指针越界".to_string());
        }
        if len == 0 {
            return Ok(pos + 1);
        }
        pos += 1 + len as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cfg(json: &str) -> Option<DnsOverrideResolver> {
        let file: DnsOverrideFile = serde_json::from_str(json).unwrap();
        DnsOverrideResolver::from_config(&file)
    }

    #[test]
    fn build_query_wire_format() {
        let pkt = build_query("ark.cn-beijing.volces.com", RecordType::A).unwrap();
        assert_eq!(&pkt[2..4], &[0x01, 0x00]);
        assert!(pkt.windows(4).any(|w| w == [3, b'a', b'r', b'k']));
        assert_eq!(&pkt[pkt.len() - 4..], &[0, 1, 0, 1]); // A + IN
    }

    #[test]
    fn parse_baidu_answer_bytes() {
        // 用 build_query 造 ID 后拼一个两答案应答（A 1.2.3.4 + AAAA ::1）
        let mut resp = vec![0xab, 0xcd, 0x81, 0x80, 0, 1, 0, 2, 0, 0, 0, 0];
        resp.extend(build_query("a.com", RecordType::A).unwrap()[12..].to_vec());
        resp.extend_from_slice(&[0xc0, 0x0c]); // 指针到 QNAME
        resp.extend_from_slice(&[0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        resp.extend_from_slice(&[1, 2, 3, 4]);
        resp.extend_from_slice(&[0xc0, 0x0c]);
        resp.extend_from_slice(&[0, 28, 0, 1, 0, 0, 0, 60, 0, 16]);
        resp.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        let addrs = parse_answer_addrs(&resp, RecordType::A).unwrap();
        assert_eq!(addrs, vec!["1.2.3.4".parse::<IpAddr>().unwrap()]);
        let all = parse_answer_addrs(&resp, RecordType::AAAA).unwrap();
        assert_eq!(all, vec!["::1".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn rcode_error_propagates() {
        let resp = vec![0xab, 0xcd, 0x81, 0x83, 0, 0, 0, 0, 0, 0, 0, 0]; // rcode=3 NXDOMAIN
        assert!(parse_answer_addrs(&resp, RecordType::A).is_err());
    }

    #[test]
    fn static_map_is_level2_when_nameservers_dead() {
        let resolver = sample_cfg(
            r#"{"enabled":true,"nameservers":["127.0.0.1:1"],"query_timeout_ms":80,
                "static":{"ark.cn-beijing.volces.com":["14.103.169.114"]}}"#,
        )
        .unwrap();
        let addrs = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(resolver.resolve_chain("ark.cn-beijing.volces.com"))
            .unwrap();
        assert_eq!(addrs, vec!["14.103.169.114".parse::<IpAddr>().unwrap()]);
    }

    #[test]
    fn empty_cfg_disables_feature() {
        assert!(sample_cfg(r#"{"enabled":true}"#).is_none());
        assert!(sample_cfg(r#"{"enabled":false,"nameservers":["223.5.5.5"]}"#).is_none());
    }

    #[tokio::test]
    async fn live_public_dns_query() {
        // 需要真实网络；CI 断网环境用 -- --ignored 跳过
        let resolver =
            sample_cfg(r#"{"enabled":true,"nameservers":["223.5.5.5"],"query_timeout_ms":2000}"#)
                .unwrap();
        let addrs = resolver.resolve_chain("www.baidu.com").await.unwrap();
        assert!(!addrs.is_empty());
    }
}
