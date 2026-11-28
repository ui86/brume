use super::ResolverSnapshot;
use hickory_resolver::Hosts;
use hickory_resolver::config::{ResolverConfig, ResolverOpts};
use std::sync::Arc;

pub(super) fn read_hosts() -> (Arc<Hosts>, Vec<u8>) {
    #[cfg(unix)]
    let source = std::fs::read("/etc/hosts").unwrap_or_default();
    #[cfg(windows)]
    let source = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .and_then(|root| std::fs::read(root.join("System32\\drivers\\etc\\hosts")).ok())
        .unwrap_or_default();
    #[cfg(not(any(unix, windows)))]
    let source = Vec::new();
    let mut hosts = Hosts::default();
    if hosts.read_hosts_conf(source.as_slice()).is_err() {
        hosts = Hosts::default();
    }
    (Arc::new(hosts), source)
}

impl ResolverSnapshot {
    pub(super) fn same_sources(&self, other: &Self) -> bool {
        let hosts_equal = match (&self.hosts_source, &other.hosts_source) {
            (Some(a), Some(b)) => a == b,
            _ => Arc::ptr_eq(&self.hosts, &other.hosts),
        };
        let errors_equal = match (&self.configuration_error, &other.configuration_error) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.kind() == b.kind()
                    && a.raw_os_error() == b.raw_os_error()
                    && a.to_string() == b.to_string()
            }
            _ => false,
        };
        hosts_equal
            && errors_equal
            && same_config(&self.config, &other.config)
            && same_options(&self.options, &other.options)
    }
}

fn same_config(a: &ResolverConfig, b: &ResolverConfig) -> bool {
    a.domain == b.domain
        && a.search == b.search
        && a.name_servers.len() == b.name_servers.len()
        && a.name_servers.iter().zip(&b.name_servers).all(|(a, b)| {
            a.ip == b.ip
                && a.trust_negative_responses == b.trust_negative_responses
                && a.connections.len() == b.connections.len()
                && a.connections.iter().zip(&b.connections).all(|(a, b)| {
                    a.port == b.port && a.protocol == b.protocol && a.bind_addr == b.bind_addr
                })
        })
}

fn same_options(a: &ResolverOpts, b: &ResolverOpts) -> bool {
    // Hickory 配置未实现相等比较，逐项覆盖当前依赖版本的公开解析选项。
    a.ndots == b.ndots
        && a.timeout == b.timeout
        && a.attempts == b.attempts
        && a.edns0 == b.edns0
        && a.ip_strategy == b.ip_strategy
        && a.cache_size == b.cache_size
        && a.use_hosts_file == b.use_hosts_file
        && a.positive_min_ttl == b.positive_min_ttl
        && a.negative_min_ttl == b.negative_min_ttl
        && a.positive_max_ttl == b.positive_max_ttl
        && a.negative_max_ttl == b.negative_max_ttl
        && a.num_concurrent_reqs == b.num_concurrent_reqs
        && a.max_active_requests == b.max_active_requests
        && a.preserve_intermediates == b.preserve_intermediates
        && a.try_tcp_on_error == b.try_tcp_on_error
        && a.server_ordering_strategy == b.server_ordering_strategy
        && a.recursion_desired == b.recursion_desired
        && a.avoid_local_udp_ports == b.avoid_local_udp_ports
        && a.os_port_selection == b.os_port_selection
        && a.case_randomization == b.case_randomization
        && a.trust_anchor == b.trust_anchor
        && a.allow_answers == b.allow_answers
        && a.deny_answers == b.deny_answers
        && a.edns_payload_len == b.edns_payload_len
}
