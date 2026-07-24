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
    /// Count ConnectionRefused as success: a RST proves the host is alive
    /// even when the probed port is closed (e.g. router admin ports).
    pub alive_on_refused: bool,
}

impl TcpPinger {
    pub async fn ping(&self) -> PingSample {
        let target = format!("{}:{}", self.addr, self.port);
        let dur = Duration::from_millis(self.timeout_ms);
        let start = std::time::Instant::now();
        let res = timeout(dur, TcpStream::connect(&target)).await;
        let rtt = match res {
            Ok(Ok(_s)) => Some(start.elapsed().as_secs_f64() * 1000.0),
            Ok(Err(e))
                if self.alive_on_refused && e.kind() == std::io::ErrorKind::ConnectionRefused =>
            {
                Some(start.elapsed().as_secs_f64() * 1000.0)
            }
            _ => None,
        };
        PingSample { rtt_ms: rtt }
    }
}

pub struct DnsProbe {
    domain: String,
    timeout_ms: u64,
    resolver: std::sync::Arc<hickory_resolver::TokioResolver>,
}

impl DnsProbe {
    pub async fn system(domain: &str, timeout_ms: u64) -> Option<Self> {
        let mut builder = hickory_resolver::TokioResolver::builder_tokio().ok()?;
        {
            let opts = builder.options_mut();
            opts.timeout = Duration::from_millis(timeout_ms);
            opts.attempts = 1;
            opts.cache_size = 0;
        }
        let resolver = builder.build().ok()?;
        Some(Self {
            domain: domain.to_string(),
            timeout_ms,
            resolver: std::sync::Arc::new(resolver),
        })
    }

    pub fn custom(domain: &str, ns_ip: &str, timeout_ms: u64) -> Option<Self> {
        use hickory_resolver::config::{NameServerConfig, ResolverConfig};
        use hickory_resolver::net::runtime::TokioRuntimeProvider;
        let ip: std::net::IpAddr = ns_ip.parse().ok()?;
        let ns = NameServerConfig::udp(ip);
        let config = ResolverConfig::from_parts(None, vec![], vec![ns]);
        let mut builder = hickory_resolver::Resolver::builder_with_config(
            config,
            TokioRuntimeProvider::default(),
        );
        {
            let opts = builder.options_mut();
            opts.timeout = Duration::from_millis(timeout_ms);
            opts.attempts = 1;
            opts.cache_size = 0;
        }
        let resolver = builder.build().ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn closed_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }

    #[tokio::test]
    async fn refused_counts_as_alive_when_flag_set() {
        let p = TcpPinger {
            addr: "127.0.0.1".into(),
            port: closed_port(),
            timeout_ms: 500,
            alive_on_refused: true,
        };
        assert!(p.ping().await.rtt_ms.is_some());
    }

    #[tokio::test]
    async fn refused_counts_as_loss_by_default() {
        let p = TcpPinger {
            addr: "127.0.0.1".into(),
            port: closed_port(),
            timeout_ms: 500,
            alive_on_refused: false,
        };
        assert!(p.ping().await.rtt_ms.is_none());
    }
}
