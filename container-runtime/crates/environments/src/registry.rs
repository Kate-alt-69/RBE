//! Ties the six environments together: five general ones sharing the
//! same health/abuse-detection shape, plus the payment environment
//! (its own health/abuse tracking too — no exception from monitoring
//! just because it's the payment one — wrapping [`payment::PaymentEnvironment`]
//! for the encryption behavior general environments don't need).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use atomic_io::AtomicIo;

use crate::abuse::{AbuseDetector, AbuseThresholds, AbuseVerdict};
use crate::health::{HealthMonitor, HealthStatus, HealthThresholds};
use crate::id::EnvironmentId;
use crate::payment::PaymentEnvironment;

struct GeneralEnvironment {
    health: Mutex<HealthMonitor>,
    abuse: AbuseDetector,
}

pub struct EnvironmentRegistry {
    general: HashMap<EnvironmentId, GeneralEnvironment>,
    payment_health: Mutex<HealthMonitor>,
    payment_abuse: AbuseDetector,
    pub payment: PaymentEnvironment,
}

impl EnvironmentRegistry {
    pub fn new(io: AtomicIo, vault: Arc<vault::Vault>, data_dir: &Path) -> Self {
        let mut general = HashMap::new();
        for id in EnvironmentId::GENERAL {
            general.insert(
                id,
                GeneralEnvironment {
                    health: Mutex::new(HealthMonitor::new(HealthThresholds::default())),
                    abuse: AbuseDetector::new(AbuseThresholds::default()),
                },
            );
        }

        Self {
            general,
            payment_health: Mutex::new(HealthMonitor::new(HealthThresholds::default())),
            payment_abuse: AbuseDetector::new(AbuseThresholds::payment_defaults()),
            payment: PaymentEnvironment::new(io, vault, data_dir),
        }
    }

    /// Records a completed execution in one of the five GENERAL
    /// environments. Panics if given [`EnvironmentId::Payment`] — that's
    /// a caller bug (wrong method for that environment kind), not a
    /// runtime condition to handle quietly; use
    /// [`record_payment_execution`](Self::record_payment_execution)
    /// for the payment environment instead.
    pub fn record_general_execution(
        &self,
        id: EnvironmentId,
        caller: &str,
        cpu_ms: u64,
        network_bytes: u64,
        disk_bytes: u64,
        succeeded: bool,
    ) -> AbuseVerdict {
        let env = self.general.get(&id).unwrap_or_else(|| {
            panic!(
                "record_general_execution called with {id} — use record_payment_execution for \
                 the payment environment instead"
            )
        });

        {
            let mut health = env.health.lock().unwrap();
            if succeeded {
                health.record_success();
            } else {
                health.record_failure();
            }
        }

        env.abuse.record_execution(caller, cpu_ms, network_bytes, disk_bytes)
    }

    /// Same shape as `record_general_execution`, for the payment
    /// environment specifically — call this alongside (not instead of)
    /// `payment.process_encrypted`/`payment.send_details`, once real
    /// resource usage for that call is known.
    pub fn record_payment_execution(
        &self,
        caller: &str,
        cpu_ms: u64,
        network_bytes: u64,
        disk_bytes: u64,
        succeeded: bool,
    ) -> AbuseVerdict {
        {
            let mut health = self.payment_health.lock().unwrap();
            if succeeded {
                health.record_success();
            } else {
                health.record_failure();
            }
        }

        self.payment_abuse
            .record_execution(caller, cpu_ms, network_bytes, disk_bytes)
    }

    /// Current health of all six environments — what a health-check
    /// endpoint or monitoring loop would poll.
    pub fn health_snapshot(&self) -> Vec<(EnvironmentId, HealthStatus)> {
        let mut out = Vec::with_capacity(EnvironmentId::ALL.len());
        for id in EnvironmentId::GENERAL {
            let status = self.general[&id].health.lock().unwrap().status();
            out.push((id, status));
        }
        out.push((
            EnvironmentId::Payment,
            self.payment_health.lock().unwrap().status(),
        ));
        out
    }

    /// Opportunistic cleanup for every environment's abuse-detection
    /// map — call periodically from a supervised task, same pattern as
    /// the engine's rate-limiter/IP-strike sweeps.
    pub fn sweep(&self) {
        for env in self.general.values() {
            env.abuse.sweep();
        }
        self.payment_abuse.sweep();
    }
}
