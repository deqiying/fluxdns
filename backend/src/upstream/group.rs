//! 无网络副作用的 upstream group 选择器。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;

use crate::config::model::UpstreamMode;
use crate::config::resolve::ResolvedUpstreamMember;
use crate::ports::exchange::SelectionPolicy;

#[derive(Debug)]
pub struct GroupSelector {
    mode: SelectionPolicy,
    members: Arc<[ResolvedUpstreamMember]>,
    smooth: Mutex<SmoothState>,
    load: Arc<LoadState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum GroupSelectorError {
    #[error("upstream group must contain at least one member")]
    EmptyMembers,
    #[error("upstream group member {index} has zero weight")]
    ZeroWeight { index: usize },
    #[error("upstream group member {index} is duplicated")]
    DuplicateMember { index: usize },
    #[error("selection policy `{mode:?}` is not valid for an upstream group")]
    InvalidMode { mode: SelectionPolicy },
    #[error("selection policy `{mode:?}` does not have a single primary member")]
    NoPrimary { mode: SelectionPolicy },
    #[error("upstream group member index {index} is out of range")]
    InvalidMemberIndex { index: usize },
    #[error("upstream group member {index} must have weight 1 for `{mode:?}`")]
    FixedWeightRequired { index: usize, mode: SelectionPolicy },
}

#[derive(Debug)]
struct SmoothState {
    current: Vec<i128>,
    total_weight: i128,
}

#[derive(Debug)]
struct LoadState {
    in_flight: Vec<AtomicU64>,
    cursor: AtomicU64,
    selection_lock: Mutex<()>,
}

pub struct SelectionLease {
    state: Arc<LoadState>,
    member_index: usize,
}

impl std::fmt::Debug for SelectionLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SelectionLease")
            .field("member_index", &self.member_index)
            .field("in_flight", &self.in_flight())
            .finish()
    }
}

impl Drop for SelectionLease {
    fn drop(&mut self) {
        let _ = self.state.in_flight[self.member_index].fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |value| value.checked_sub(1),
        );
    }
}

impl SelectionLease {
    pub fn member_index(&self) -> usize {
        self.member_index
    }

    pub fn in_flight(&self) -> u64 {
        self.state.in_flight[self.member_index].load(Ordering::Acquire)
    }
}

impl GroupSelector {
    pub fn new(
        mode: SelectionPolicy,
        members: Vec<ResolvedUpstreamMember>,
    ) -> Result<Self, GroupSelectorError> {
        if members.is_empty() {
            return Err(GroupSelectorError::EmptyMembers);
        }
        if matches!(mode, SelectionPolicy::Sequential) {
            return Err(GroupSelectorError::InvalidMode { mode });
        }

        let mut current = Vec::with_capacity(members.len());
        let mut total_weight = 0_i128;
        for (index, member) in members.iter().enumerate() {
            if member.weight == 0 {
                return Err(GroupSelectorError::ZeroWeight { index });
            }
            if members[..index]
                .iter()
                .any(|previous| previous.name == member.name)
            {
                return Err(GroupSelectorError::DuplicateMember { index });
            }
            if matches!(mode, SelectionPolicy::Parallel | SelectionPolicy::Failover)
                && member.weight != 1
            {
                return Err(GroupSelectorError::FixedWeightRequired { index, mode });
            }
            let weight = i128::from(member.weight);
            current.push(0);
            total_weight += weight;
        }

        let member_count = members.len();
        Ok(Self {
            mode,
            members: members.into(),
            smooth: Mutex::new(SmoothState {
                current,
                total_weight,
            }),
            load: Arc::new(LoadState {
                in_flight: (0..member_count).map(|_| AtomicU64::new(0)).collect(),
                cursor: AtomicU64::new(0),
                selection_lock: Mutex::new(()),
            }),
        })
    }

    pub fn from_upstream_mode(
        mode: UpstreamMode,
        members: Vec<ResolvedUpstreamMember>,
    ) -> Result<Self, GroupSelectorError> {
        Self::new(mode.into(), members)
    }

    pub fn mode(&self) -> SelectionPolicy {
        self.mode
    }

    pub fn members(&self) -> &[ResolvedUpstreamMember] {
        &self.members
    }

    pub fn select_primary(&self) -> Result<usize, GroupSelectorError> {
        match self.mode {
            SelectionPolicy::RoundRobin => {
                let mut state = lock_unpoisoned(&self.smooth);
                let selected = smooth_select(&mut state, &self.members);
                Ok(selected)
            }
            SelectionPolicy::LoadBalance => Ok(self.load_balance_index()),
            SelectionPolicy::Failover => Ok(0),
            SelectionPolicy::Parallel => Err(GroupSelectorError::NoPrimary { mode: self.mode }),
            SelectionPolicy::Sequential => Err(GroupSelectorError::InvalidMode { mode: self.mode }),
        }
    }

    pub fn acquire_primary(&self) -> Result<SelectionLease, GroupSelectorError> {
        if !matches!(self.mode, SelectionPolicy::LoadBalance) {
            return Err(GroupSelectorError::InvalidMode { mode: self.mode });
        }
        let _guard = lock_unpoisoned(&self.load.selection_lock);
        let index = self.load_balance_index();
        self.load.in_flight[index].fetch_add(1, Ordering::AcqRel);
        Ok(SelectionLease {
            state: Arc::clone(&self.load),
            member_index: index,
        })
    }

    pub fn in_flight(&self, index: usize) -> Result<u64, GroupSelectorError> {
        self.load
            .in_flight
            .get(index)
            .map(|value| value.load(Ordering::Acquire))
            .ok_or(GroupSelectorError::InvalidMemberIndex { index })
    }

    pub fn ordered_candidates(&self, primary: usize) -> Result<Vec<usize>, GroupSelectorError> {
        if primary >= self.members.len() {
            return Err(GroupSelectorError::InvalidMemberIndex { index: primary });
        }
        Ok((0..self.members.len())
            .filter(|index| *index != primary)
            .collect())
    }

    pub fn parallel_order(&self) -> Vec<usize> {
        (0..self.members.len()).collect()
    }

    fn load_balance_index(&self) -> usize {
        let start = self.load.cursor.fetch_add(1, Ordering::AcqRel) as usize % self.members.len();
        let mut best = start;
        for offset in 1..self.members.len() {
            let candidate = (start + offset) % self.members.len();
            if load_ratio_is_less(
                self.load.in_flight[candidate].load(Ordering::Acquire),
                self.members[candidate].weight,
                self.load.in_flight[best].load(Ordering::Acquire),
                self.members[best].weight,
            ) {
                best = candidate;
            }
        }
        best
    }
}

impl From<UpstreamMode> for SelectionPolicy {
    fn from(mode: UpstreamMode) -> Self {
        match mode {
            UpstreamMode::Parallel => Self::Parallel,
            UpstreamMode::RoundRobin => Self::RoundRobin,
            UpstreamMode::LoadBalance => Self::LoadBalance,
            UpstreamMode::Failover => Self::Failover,
        }
    }
}

fn smooth_select(state: &mut SmoothState, members: &[ResolvedUpstreamMember]) -> usize {
    let mut selected = 0;
    for (index, member) in members.iter().enumerate() {
        state.current[index] += i128::from(member.weight);
        if state.current[index] > state.current[selected] {
            selected = index;
        }
    }
    state.current[selected] -= state.total_weight;
    selected
}

fn load_ratio_is_less(
    candidate_in_flight: u64,
    candidate_weight: u32,
    current_in_flight: u64,
    current_weight: u32,
) -> bool {
    u128::from(candidate_in_flight) * u128::from(current_weight)
        < u128::from(current_in_flight) * u128::from(candidate_weight)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use crate::config::resolve::ConfigId;
    use crate::ports::exchange::SelectionPolicy;

    use super::{GroupSelector, GroupSelectorError};

    fn members(weights: &[u32]) -> Vec<crate::config::resolve::ResolvedUpstreamMember> {
        weights
            .iter()
            .enumerate()
            .map(
                |(index, weight)| crate::config::resolve::ResolvedUpstreamMember {
                    name: ConfigId::new(format!("upstream-{index}")).unwrap(),
                    weight: *weight,
                },
            )
            .collect()
    }

    #[test]
    fn rejects_empty_zero_weight_fixed_weight_and_invalid_modes() {
        assert!(matches!(
            GroupSelector::new(SelectionPolicy::RoundRobin, Vec::new()),
            Err(GroupSelectorError::EmptyMembers)
        ));
        assert!(matches!(
            GroupSelector::new(SelectionPolicy::RoundRobin, members(&[0])),
            Err(GroupSelectorError::ZeroWeight { index: 0 })
        ));
        assert!(matches!(
            GroupSelector::new(SelectionPolicy::Parallel, members(&[2, 1])),
            Err(GroupSelectorError::FixedWeightRequired {
                index: 0,
                mode: SelectionPolicy::Parallel,
            })
        ));
        assert!(matches!(
            GroupSelector::new(SelectionPolicy::Sequential, members(&[1])),
            Err(GroupSelectorError::InvalidMode {
                mode: SelectionPolicy::Sequential,
            })
        ));
        let duplicate = vec![
            crate::config::resolve::ResolvedUpstreamMember {
                name: ConfigId::new("same").unwrap(),
                weight: 1,
            },
            crate::config::resolve::ResolvedUpstreamMember {
                name: ConfigId::new("same").unwrap(),
                weight: 1,
            },
        ];
        assert!(matches!(
            GroupSelector::new(SelectionPolicy::RoundRobin, duplicate),
            Err(GroupSelectorError::DuplicateMember { index: 1 })
        ));
    }

    #[test]
    fn round_robin_has_deterministic_smooth_weighted_distribution() {
        let selector = GroupSelector::new(SelectionPolicy::RoundRobin, members(&[2, 1])).unwrap();
        let selected = (0..6)
            .map(|_| selector.select_primary().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(selected, vec![0, 1, 0, 0, 1, 0]);
    }

    #[test]
    fn failover_and_candidate_order_preserve_configuration_order() {
        let selector = GroupSelector::new(SelectionPolicy::Failover, members(&[1, 1, 1])).unwrap();
        assert_eq!(selector.select_primary().unwrap(), 0);
        assert_eq!(selector.ordered_candidates(0).unwrap(), vec![1, 2]);
        assert_eq!(selector.parallel_order(), vec![0, 1, 2]);
        assert_eq!(
            selector.ordered_candidates(3),
            Err(GroupSelectorError::InvalidMemberIndex { index: 3 })
        );
    }

    #[test]
    fn load_balance_uses_ratio_and_round_robin_tie_breaking() {
        let selector = GroupSelector::new(SelectionPolicy::LoadBalance, members(&[1, 2])).unwrap();
        let first = selector.acquire_primary().unwrap();
        assert_eq!(first.member_index(), 0);
        let second = selector.acquire_primary().unwrap();
        assert_eq!(second.member_index(), 1);
        assert_eq!(selector.in_flight(0), Ok(1));
        assert_eq!(selector.in_flight(1), Ok(1));
        drop(first);
        assert_eq!(selector.in_flight(0), Ok(0));
        let third = selector.acquire_primary().unwrap();
        assert_eq!(third.member_index(), 0);
    }

    #[test]
    fn concurrent_leases_are_counted_and_released_once() {
        let selector =
            Arc::new(GroupSelector::new(SelectionPolicy::LoadBalance, members(&[1, 1])).unwrap());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let selector = Arc::clone(&selector);
            handles.push(thread::spawn(move || {
                let lease = selector.acquire_primary().unwrap();
                lease.member_index()
            }));
        }
        let indexes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(indexes.len(), 8);
        assert_eq!(selector.in_flight(0), Ok(0));
        assert_eq!(selector.in_flight(1), Ok(0));
    }
}
