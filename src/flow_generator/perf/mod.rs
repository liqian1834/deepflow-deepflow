mod dns;
mod http;
pub(crate) mod l7_rrt;
mod mq;
mod rpc;
mod sql;
mod stats;
pub mod tcp;
mod udp;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use enum_dispatch::enum_dispatch;

use crate::common::{
    flow::{FlowPerfStats, L4Protocol, L7Protocol},
    meta_packet::MetaPacket,
    protocol_logs::AppProtoHead,
};
use crate::flow_generator::error::Result;
use crate::utils::stats::{Countable, Counter, CounterType, CounterValue};

use {
    dns::DNS_PORT, mq::KAFKA_PORT, rpc::DUBBO_PORT, sql::MYSQL_PORT, sql::REDIS_PORT, tcp::TcpPerf,
    udp::UdpPerf,
};

pub use l7_rrt::L7RrtCache;

const ART_MAX: Duration = Duration::from_secs(30);

pub struct DNSPerfData {
    rrt_cache: Rc<RefCell<L7RrtCache>>,
}

impl DNSPerfData {
    pub fn new(rrt_cache: Rc<RefCell<L7RrtCache>>) -> Self {
        Self { rrt_cache }
    }
}

impl L7FlowPerf for DNSPerfData {
    // rrt_cache 作为实现者一个字段，在构造实现者的时候传入,失败返回错误信息和missed_match_response_count
    fn parse(&mut self, _: &MetaPacket, _: u64) -> Result<()> {
        Ok(())
    }

    fn data_updated(&self) -> bool {
        false
    }

    fn copy_and_reset_data(&mut self, _: u32) -> FlowPerfStats {
        FlowPerfStats::default()
    }

    fn app_proto_head(&mut self) -> Option<(AppProtoHead, u16)> {
        None
    }
}
//END

#[enum_dispatch(L4FlowPerfTable)]
pub trait L4FlowPerf {
    fn parse(&mut self, packet: &MetaPacket, direction: bool) -> Result<()>;
    fn data_updated(&self) -> bool;
    fn copy_and_reset_data(&mut self, flow_reversed: bool) -> FlowPerfStats;
}

#[enum_dispatch(L7FlowPerfTable)]
pub trait L7FlowPerf {
    fn parse(&mut self, packet: &MetaPacket, flow_id: u64) -> Result<()>;
    fn data_updated(&self) -> bool;
    fn copy_and_reset_data(&mut self, l7_timeout_count: u32) -> FlowPerfStats;
    fn app_proto_head(&mut self) -> Option<(AppProtoHead, u16)>;
}

#[enum_dispatch]
pub enum L4FlowPerfTable {
    TcpPerf,
    UdpPerf,
}

#[enum_dispatch]
pub enum L7FlowPerfTable {
    DNSPerfData,
}

#[derive(Default)]
pub struct FlowPerfCounter {
    closed: AtomicBool,

    // tcp stats
    pub ignored_packet_count: AtomicU64,
    pub invalid_packet_count: AtomicU64,

    // L7 stats
    pub mismatched_response: AtomicU64,
}

impl Countable for FlowPerfCounter {
    fn get_counters(&self) -> Vec<Counter> {
        let ignored = self.ignored_packet_count.swap(0, Ordering::Relaxed);
        let invalid = self.invalid_packet_count.swap(0, Ordering::Relaxed);
        let mismatched = self.mismatched_response.swap(0, Ordering::Relaxed);

        vec![
            (
                "ignore_packet_count",
                CounterType::Counted,
                CounterValue::Unsigned(ignored),
            ),
            (
                "invalid_packet_count",
                CounterType::Counted,
                CounterValue::Unsigned(invalid),
            ),
            (
                "l7_mismatch_response",
                CounterType::Counted,
                CounterValue::Unsigned(mismatched),
            ),
        ]
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

pub struct FlowPerf {
    l4: L4FlowPerfTable,
    l7: L7FlowPerfTable,
}

impl FlowPerf {
    pub fn new(
        rrt_cache: Rc<RefCell<L7RrtCache>>,
        l4_proto: L4Protocol,
        l7_proto: L7Protocol,
        counter: Arc<FlowPerfCounter>,
    ) -> Option<Self> {
        let l4 = match l4_proto {
            L4Protocol::Tcp => L4FlowPerfTable::from(TcpPerf::new(counter)),
            L4Protocol::Udp => L4FlowPerfTable::from(UdpPerf::new()),
            _ => {
                return None;
            }
        };

        let l7 = match l7_proto {
            L7Protocol::Dns => L7FlowPerfTable::from(DNSPerfData::new(rrt_cache.clone())),
            _ => unimplemented!(),
        };

        Some(Self { l4, l7 })
    }

    pub fn parse(
        &mut self,
        packet: &MetaPacket,
        is_first_packet_direction: bool,
        flow_id: u64,
        l4_performance_enabled: bool,
        l7_performance_enabled: bool,
    ) -> Result<()> {
        if l4_performance_enabled {
            self.l4.parse(packet, is_first_packet_direction)?;
        }
        if l7_performance_enabled {
            // 抛出错误由flowMap.FlowPerfCounter处理
            self.l7.parse(packet, flow_id)?;
        }
        Ok(())
    }

    pub fn copy_and_reset_perf_data(
        &mut self,
        flow_reversed: bool,
        l7_timeout_count: u32,
        l4_performance_enabled: bool,
        l7_performance_enabled: bool,
    ) -> Option<FlowPerfStats> {
        if !l4_performance_enabled {
            return None;
        }

        let mut stats = None;
        if self.l4.data_updated() {
            stats.replace(self.l4.copy_and_reset_data(flow_reversed));
        }

        if l7_performance_enabled && self.l7.data_updated() {
            if let Some(stats) = stats.as_mut() {
                let FlowPerfStats {
                    l7, l7_protocol, ..
                } = self.l7.copy_and_reset_data(l7_timeout_count);

                stats.l7 = l7;
                stats.l7_protocol = l7_protocol;
            }
        }

        stats
    }

    pub fn app_proto_head(&mut self, l7_performance_enabled: bool) -> Option<(AppProtoHead, u16)> {
        if !l7_performance_enabled {
            return None;
        }
        self.l7.app_proto_head()
    }
}

pub fn get_l7_protocol(src_port: u16, dst_port: u16, l7_performance_enabled: bool) -> L7Protocol {
    if !l7_performance_enabled {
        return L7Protocol::Unknown;
    }

    if src_port == DNS_PORT || dst_port == DNS_PORT {
        return L7Protocol::Dns;
    }

    if src_port == MYSQL_PORT || dst_port == MYSQL_PORT {
        return L7Protocol::Mysql;
    }

    if src_port == REDIS_PORT || dst_port == REDIS_PORT {
        return L7Protocol::Redis;
    }

    if src_port == DUBBO_PORT || dst_port == DUBBO_PORT {
        return L7Protocol::Dubbo;
    }

    if src_port == KAFKA_PORT || dst_port == KAFKA_PORT {
        return L7Protocol::Kafka;
    }

    L7Protocol::Http
}
