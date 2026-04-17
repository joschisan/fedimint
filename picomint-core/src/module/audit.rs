use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use itertools::Itertools;
use picomint_core::core::ModuleKind;
use serde::{Deserialize, Serialize};

#[derive(Default)]
pub struct Audit {
    items: Vec<AuditItem>,
}

impl Audit {
    pub fn net_assets(&self) -> Option<AuditItem> {
        Some(AuditItem {
            name: "Net assets (sats)".to_string(),
            milli_sat: calculate_net_assets(self.items.iter())?,
            module_kind: None,
        })
    }

    /// Add pre-computed `(name, milli_sat)` items for a module. Modules are
    /// expected to iterate their own tables and pass the resulting
    /// (row-name, signed-milli-sat) pairs here.
    pub fn add_items(
        &mut self,
        module_kind: ModuleKind,
        items: impl IntoIterator<Item = (String, i64)>,
    ) {
        self.items
            .extend(items.into_iter().map(|(name, milli_sat)| AuditItem {
                name,
                milli_sat,
                module_kind: Some(module_kind),
            }));
    }
}

impl Display for Audit {
    fn fmt(&self, formatter: &mut Formatter) -> std::fmt::Result {
        formatter.write_str("- Balance Sheet -")?;
        for item in &self.items {
            formatter.write_fmt(format_args!("\n{item}"))?;
        }
        formatter.write_fmt(format_args!(
            "\n{}",
            self.net_assets()
                .expect("We'd have crashed already if there was an overflow")
        ))
    }
}

pub struct AuditItem {
    pub name: String,
    pub milli_sat: i64,
    pub module_kind: Option<ModuleKind>,
}

impl Display for AuditItem {
    fn fmt(&self, formatter: &mut Formatter) -> std::fmt::Result {
        let sats = (self.milli_sat as f64) / 1000.0;
        formatter.write_fmt(format_args!("{:>+15.3}|{}", sats, self.name))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AuditSummary {
    pub net_assets: i64,
    pub module_summaries: BTreeMap<ModuleKind, ModuleSummary>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModuleSummary {
    pub net_assets: i64,
}

impl AuditSummary {
    pub fn from_audit(audit: &Audit) -> Self {
        let placeholders = [ModuleKind::Mint, ModuleKind::Ln, ModuleKind::Wallet]
            .into_iter()
            .map(|kind| AuditItem {
                name: "Module placeholder".to_string(),
                milli_sat: 0,
                module_kind: Some(kind),
            })
            .collect::<Vec<_>>();
        Self {
            net_assets: calculate_net_assets(audit.items.iter())
                .expect("We'd have crashed already if there was an overflow"),
            module_summaries: generate_module_summaries(
                audit.items.iter().chain(&placeholders),
            ),
        }
    }
}

fn generate_module_summaries<'a>(
    audit_items: impl Iterator<Item = &'a AuditItem>,
) -> BTreeMap<ModuleKind, ModuleSummary> {
    audit_items
        .filter_map(|item| item.module_kind.map(|kind| (kind, item)))
        .into_group_map()
        .into_iter()
        .map(|(kind, module_audit_items)| {
            (
                kind,
                ModuleSummary {
                    net_assets: calculate_net_assets(module_audit_items.into_iter())
                        .expect("We'd have crashed already if there was an overflow"),
                },
            )
        })
        .collect()
}

fn calculate_net_assets<'a>(items: impl Iterator<Item = &'a AuditItem>) -> Option<i64> {
    items
        .map(|item| item.milli_sat)
        .try_fold(0i64, i64::checked_add)
}

#[test]
fn creates_audit_summary_from_audit() {
    let audit = Audit {
        items: vec![
            AuditItem {
                name: "ContractKey(...)".to_string(),
                milli_sat: -101_000,
                module_kind: Some(ModuleKind::Ln),
            },
            AuditItem {
                name: "IssuanceTotal".to_string(),
                milli_sat: -50_100_000,
                module_kind: Some(ModuleKind::Mint),
            },
            AuditItem {
                name: "Redemption(...)".to_string(),
                milli_sat: 101_000,
                module_kind: Some(ModuleKind::Mint),
            },
            AuditItem {
                name: "RedemptionTotal".to_string(),
                milli_sat: 100_000,
                module_kind: Some(ModuleKind::Mint),
            },
            AuditItem {
                name: "UTXOKey(...)".to_string(),
                milli_sat: 20_000_000,
                module_kind: Some(ModuleKind::Wallet),
            },
            AuditItem {
                name: "UTXOKey(...)".to_string(),
                milli_sat: 10_000_000,
                module_kind: Some(ModuleKind::Wallet),
            },
            AuditItem {
                name: "UTXOKey(...)".to_string(),
                milli_sat: 20_000_000,
                module_kind: Some(ModuleKind::Wallet),
            },
        ],
    };

    let audit_summary = AuditSummary::from_audit(&audit);
    let expected_audit_summary = AuditSummary {
        net_assets: 0,
        module_summaries: BTreeMap::from_iter([
            (ModuleKind::Mint, ModuleSummary { net_assets: -49_899_000 }),
            (ModuleKind::Ln, ModuleSummary { net_assets: -101_000 }),
            (ModuleKind::Wallet, ModuleSummary { net_assets: 50_000_000 }),
        ]),
    };

    assert_eq!(audit_summary, expected_audit_summary);
}

#[test]
fn audit_summary_includes_placeholders() {
    let audit_summary = AuditSummary::from_audit(&Audit::default());
    let expected_audit_summary = AuditSummary {
        net_assets: 0,
        module_summaries: BTreeMap::from_iter([
            (ModuleKind::Mint, ModuleSummary { net_assets: 0 }),
            (ModuleKind::Ln, ModuleSummary { net_assets: 0 }),
            (ModuleKind::Wallet, ModuleSummary { net_assets: 0 }),
        ]),
    };

    assert_eq!(audit_summary, expected_audit_summary);
}
