//! 仅在管理面显式请求时分析不可变规则载荷，不读取文件、不进入 DNS 热路径。

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::Arc;
use std::time::Instant;

use thiserror::Error;

use super::RuleIndex;

const MAX_ROWS: usize = 4096;
const MAX_VISITS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum RuleMemoryHolder {
    Initial,
    Worker,
    Policy,
}

#[derive(Clone, Debug)]
pub(crate) struct RuleMemoryRoot {
    pub resource: String,
    pub holder: RuleMemoryHolder,
    pub index: Arc<RuleIndex>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct RuleMemoryReport {
    pub estimated_bytes: u64,
    pub unique_indexes: u64,
    pub retained_indexes: u64,
    pub rows: Vec<RuleMemoryRow>,
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct RuleMemoryRow {
    pub resource: String,
    pub selector: Option<String>,
    pub rule_count: u64,
    pub estimated_bytes: u64,
    pub holders: Vec<RuleMemoryHolder>,
    pub retained: bool,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum RuleMemoryError {
    #[error("rule memory analysis deadline exceeded")]
    DeadlineExceeded,
    #[error("rule memory analysis traversal budget exceeded")]
    BudgetExceeded,
    #[error("rule memory analysis row limit exceeded")]
    RowLimitExceeded,
    #[error("rule memory analysis numeric overflow")]
    Overflow,
}

pub(crate) struct MemoryBudget {
    deadline: Instant,
    remaining: u64,
}

impl MemoryBudget {
    fn check(&self) -> Result<(), RuleMemoryError> {
        if Instant::now() >= self.deadline {
            return Err(RuleMemoryError::DeadlineExceeded);
        }
        Ok(())
    }

    pub(crate) fn step(&mut self) -> Result<(), RuleMemoryError> {
        self.check()?;
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or(RuleMemoryError::BudgetExceeded)?;
        Ok(())
    }

    pub(crate) fn add_array<T>(
        &self,
        total: &mut u64,
        count: usize,
    ) -> Result<(), RuleMemoryError> {
        self.check()?;
        let count = u64::try_from(count).map_err(|_| RuleMemoryError::Overflow)?;
        let size = u64::try_from(size_of::<T>()).map_err(|_| RuleMemoryError::Overflow)?;
        let bytes = count.checked_mul(size).ok_or(RuleMemoryError::Overflow)?;
        *total = total.checked_add(bytes).ok_or(RuleMemoryError::Overflow)?;
        Ok(())
    }
}

/// 按真实 Arc identity 去重，包括 DAT 共享子索引；失败时不返回部分报告。
/// 行仅归属字典序最小的 resource/selector，holders 则合并所有可达根。
/// retained 仅表示 initial 独占可达，不表示所有进程内引用或完整堆占用。
pub(crate) fn analyze_rule_memory(
    roots: Vec<RuleMemoryRoot>,
    deadline: Instant,
) -> Result<RuleMemoryReport, RuleMemoryError> {
    analyze_with_budget(roots, deadline, MAX_VISITS)
}

fn analyze_with_budget(
    roots: Vec<RuleMemoryRoot>,
    deadline: Instant,
    visits: u64,
) -> Result<RuleMemoryReport, RuleMemoryError> {
    let mut budget = MemoryBudget {
        deadline,
        remaining: visits,
    };
    let mut identities: HashMap<*const RuleIndex, usize> = HashMap::new();
    let mut rows: Vec<RuleMemoryRow> = Vec::new();
    // roots 一直存活，地址只用作本次去重键，不排序、不序列化，也不暴露给调用方。
    for root in &roots {
        let mut pending: Vec<(&RuleIndex, Option<String>)> = vec![(root.index.as_ref(), None)];
        while let Some((index, selector)) = pending.pop() {
            budget.step()?;
            let identity = index as *const RuleIndex;
            let row_index = if let Some(&position) = identities.get(&identity) {
                position
            } else {
                if rows.len() >= MAX_ROWS {
                    return Err(RuleMemoryError::RowLimitExceeded);
                }
                let position = rows.len();
                rows.push(RuleMemoryRow {
                    resource: root.resource.clone(),
                    selector: selector.clone(),
                    rule_count: u64::try_from(index.memory_rule_count())
                        .map_err(|_| RuleMemoryError::Overflow)?,
                    estimated_bytes: index.memory_payload(&mut budget)?,
                    holders: Vec::new(),
                    retained: false,
                });
                identities.insert(identity, position);
                position
            };
            let row = &mut rows[row_index];
            if (&root.resource, &selector) < (&row.resource, &row.selector) {
                row.resource = root.resource.clone();
                row.selector = selector;
            }
            if !row.holders.contains(&root.holder) {
                row.holders.push(root.holder);
            }
            for (selector, child) in index.memory_children() {
                budget.step()?;
                pending.push((child.as_ref(), Some(selector.clone())));
            }
        }
    }
    let mut report = RuleMemoryReport {
        estimated_bytes: 0,
        unique_indexes: u64::try_from(rows.len()).map_err(|_| RuleMemoryError::Overflow)?,
        retained_indexes: 0,
        rows,
    };
    for row in &mut report.rows {
        budget.check()?;
        row.holders.sort();
        row.retained = row.holders == [RuleMemoryHolder::Initial];
        report.estimated_bytes = report
            .estimated_bytes
            .checked_add(row.estimated_bytes)
            .ok_or(RuleMemoryError::Overflow)?;
        if row.retained {
            report.retained_indexes = report
                .retained_indexes
                .checked_add(1)
                .ok_or(RuleMemoryError::Overflow)?;
        }
    }
    report.rows.sort();
    budget.check()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{DatSelection, RuleLimits};
    use std::time::Duration;

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    fn json() -> Arc<RuleIndex> {
        Arc::new(RuleIndex::parse_json(r#"{"domain":["example.com"]}"#).unwrap())
    }

    fn root(index: Arc<RuleIndex>, holder: RuleMemoryHolder) -> RuleMemoryRoot {
        RuleMemoryRoot {
            resource: "rules".into(),
            holder,
            index,
        }
    }

    #[test]
    fn json_same_arc_merges_holders_and_clone_counts_separately() {
        let index = json();
        let one = analyze_rule_memory(
            vec![root(index.clone(), RuleMemoryHolder::Initial)],
            deadline(),
        )
        .unwrap();
        let merged = analyze_rule_memory(
            vec![
                root(index.clone(), RuleMemoryHolder::Initial),
                root(index.clone(), RuleMemoryHolder::Worker),
                root(index.clone(), RuleMemoryHolder::Policy),
            ],
            deadline(),
        )
        .unwrap();
        assert_eq!(merged.unique_indexes, 1);
        assert_eq!(merged.estimated_bytes, one.estimated_bytes);
        assert_eq!(merged.rows[0].holders.len(), 3);
        assert_eq!(merged.retained_indexes, 0);
        let cloned = analyze_rule_memory(
            vec![
                root(index.clone(), RuleMemoryHolder::Initial),
                root(Arc::new((*index).clone()), RuleMemoryHolder::Policy),
            ],
            deadline(),
        )
        .unwrap();
        assert_eq!(cloned.unique_indexes, 2);
        assert_eq!(cloned.retained_indexes, 1);
        assert_eq!(cloned.rows.iter().filter(|row| row.retained).count(), 1);
    }

    // 最小 GeoSiteList：两个 selector，各有一条 full domain 规则。
    fn dat() -> Vec<u8> {
        let mut output = Vec::new();
        for selector in ["a", "b"] {
            let domain = b"example.com";
            let mut entry = vec![8, 3, 18, domain.len() as u8];
            entry.extend_from_slice(domain);
            let mut site = vec![10, 1, selector.as_bytes()[0], 18, entry.len() as u8];
            site.extend(entry);
            output.extend([10, site.len() as u8]);
            output.extend(site);
        }
        output
    }

    #[test]
    fn dat_shared_children_are_not_retained_or_counted_twice() {
        let initial = Arc::new(RuleIndex::parse_dat(&dat()).unwrap());
        let current = Arc::new((*initial).clone());
        let mut roots = vec![
            root(initial.clone(), RuleMemoryHolder::Initial),
            root(current, RuleMemoryHolder::Policy),
        ];
        let report = analyze_rule_memory(roots.clone(), deadline()).unwrap();
        assert_eq!(report.unique_indexes, 4);
        assert_eq!(report.retained_indexes, 1);
        assert!(
            report
                .rows
                .iter()
                .filter(|row| row.selector.is_some())
                .all(|row| !row.retained && row.holders.len() == 2)
        );
        roots.reverse();
        assert_eq!(report, analyze_rule_memory(roots, deadline()).unwrap());
    }

    #[test]
    fn only_loaded_selectors_appear() {
        let selected = ["a".to_owned()].into_iter().collect();
        let index = RuleIndex::parse_dat_with_limits(
            &dat(),
            RuleLimits::default(),
            DatSelection::Only(&selected),
        )
        .unwrap();
        let report = analyze_rule_memory(
            vec![root(Arc::new(index), RuleMemoryHolder::Policy)],
            deadline(),
        )
        .unwrap();
        assert_eq!(report.unique_indexes, 2);
        assert_eq!(
            report
                .rows
                .iter()
                .filter_map(|row| row.selector.as_deref())
                .collect::<Vec<_>>(),
            ["a"]
        );
    }

    #[test]
    fn timeout_budget_overflow_and_empty_roots() {
        assert_eq!(
            analyze_rule_memory(vec![root(json(), RuleMemoryHolder::Policy)], Instant::now())
                .unwrap_err(),
            RuleMemoryError::DeadlineExceeded
        );
        assert_eq!(
            analyze_with_budget(vec![root(json(), RuleMemoryHolder::Policy)], deadline(), 1)
                .unwrap_err(),
            RuleMemoryError::BudgetExceeded
        );
        let budget = MemoryBudget {
            deadline: deadline(),
            remaining: 1,
        };
        let mut total = u64::MAX;
        assert_eq!(
            budget.add_array::<u8>(&mut total, 1),
            Err(RuleMemoryError::Overflow)
        );
        assert_eq!(total, u64::MAX);
        let empty = analyze_rule_memory(Vec::new(), deadline()).unwrap();
        assert_eq!(
            (
                empty.estimated_bytes,
                empty.unique_indexes,
                empty.retained_indexes
            ),
            (0, 0, 0)
        );
        assert!(empty.rows.is_empty());
    }

    #[test]
    fn row_limit_fails_without_partial_report() {
        let roots = (0..=MAX_ROWS)
            .map(|_| root(json(), RuleMemoryHolder::Policy))
            .collect();
        assert_eq!(
            analyze_rule_memory(roots, deadline()).unwrap_err(),
            RuleMemoryError::RowLimitExceeded
        );
    }
}
