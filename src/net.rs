use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

#[derive(Debug, Clone, Copy)]
pub struct PingSample {
    pub rtt_ms: Option<f64>,
}

pub struct TcpPinger {
    pub addr: String,
    pub port: u16,
    pub timeout_ms: u64,
}

impl TcpPinger {
    pub async fn ping(&self) -> PingSample {
        let target = format!("{}:{}", self.addr, self.port);
        let dur = Duration::from_millis(self.timeout_ms);
        let start = std::time::Instant::now();
        let res = timeout(dur, TcpStream::connect(&target)).await;
        let rtt = match res {
            Ok(Ok(_s)) => Some(start.elapsed().as_secs_f64() * 1000.0),
            _ => None,
        };
        PingSample { rtt_ms: rtt }
    }
}

pub struct DnsProbe {
    domain: String,
    timeout_ms: u64,
    resolver: std::sync::Arc<hickory_resolver::TokioAsyncResolver>,
}

impl DnsProbe {
    pub async fn system(domain: &str, timeout_ms: u64) -> Option<Self> {
        let (config, mut opts) = hickory_resolver::system_conf::read_system_conf().ok()?;
        opts.timeout = Duration::from_millis(timeout_ms);
        opts.attempts = 1;
        opts.cache_size = 0;
        let resolver = hickory_resolver::TokioAsyncResolver::tokio(config, opts);
        Some(Self {
            domain: domain.to_string(),
            timeout_ms,
            resolver: std::sync::Arc::new(resolver),
        })
    }

    pub fn custom(domain: &str, ns_ip: &str, timeout_ms: u64) -> Option<Self> {
        use hickory_resolver::config::{NameServerConfig, Protocol, ResolverConfig, ResolverOpts};
        let mut config = ResolverConfig::new();
        let sock_addr: std::net::SocketAddr = format!("{}:53", ns_ip).parse().ok()?;
        config.add_name_server(NameServerConfig {
            socket_addr: sock_addr,
            protocol: Protocol::Udp,
            tls_dns_name: None,
            trust_negative_responses: false,
            bind_addr: None,
        });
        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_millis(timeout_ms);
        opts.attempts = 1;
        opts.cache_size = 0;
        let resolver = hickory_resolver::TokioAsyncResolver::tokio(config, opts);
        Some(Self {
            domain: domain.to_string(),
            timeout_ms,
            resolver: std::sync::Arc::new(resolver),
        })
    }

    pub async fn probe(&self) -> Option<f64> {
        let dur = Duration::from_millis(self.timeout_ms);
        let start = std::time::Instant::now();
        let res = timeout(dur, self.resolver.lookup_ip(&self.domain)).await;
        match res {
            Ok(Ok(ips)) if ips.iter().next().is_some() => {
                Some(start.elapsed().as_secs_f64() * 1000.0)
            }
            _ => None,
        }
    }
}
