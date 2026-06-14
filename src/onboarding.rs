use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum OnboardingAchievementId {
    FirstHarvestOrCollection,
    FirstSpawn,
    FirstBuild,
    ResourceBottleneckExplanation,
    Replay,
    Arena,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingAchievement {
    pub id: OnboardingAchievementId,
    pub stable_id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnboardingEvent {
    ResourceHarvested,
    ResourceCollected,
    DroneSpawned,
    StructureBuilt,
    ResourceBottleneckExplanationAvailable,
    ResourceBottleneckExplanationViewed,
    ReplayAvailable,
    ReplayViewed,
    ArenaAvailable,
    ArenaTried,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnboardingProgress {
    unlocked: BTreeSet<OnboardingAchievementId>,
}

pub const ONBOARDING_ACHIEVEMENTS: [OnboardingAchievement; 6] = [
    OnboardingAchievement {
        id: OnboardingAchievementId::FirstHarvestOrCollection,
        stable_id: "onboarding.first_harvest_or_collection",
        title: "First Harvest",
        description: "Harvest or collect your first resource during the 5-minute tutorial loop.",
    },
    OnboardingAchievement {
        id: OnboardingAchievementId::FirstSpawn,
        stable_id: "onboarding.first_spawn",
        title: "First Spawn",
        description: "Create your first drone from a Spawn.",
    },
    OnboardingAchievement {
        id: OnboardingAchievementId::FirstBuild,
        stable_id: "onboarding.first_build",
        title: "First Build",
        description: "Build your first structure in the tutorial room.",
    },
    OnboardingAchievement {
        id: OnboardingAchievementId::ResourceBottleneckExplanation,
        stable_id: "onboarding.resource_bottleneck_explanation",
        title: "Bottleneck Explained",
        description: "Have a resource bottleneck explanation become available or view it.",
    },
    OnboardingAchievement {
        id: OnboardingAchievementId::Replay,
        stable_id: "onboarding.replay",
        title: "Replay Ready",
        description: "Have a replay become available or view it.",
    },
    OnboardingAchievement {
        id: OnboardingAchievementId::Arena,
        stable_id: "onboarding.arena",
        title: "Arena Ready",
        description: "Have Arena become available or try it.",
    },
];

impl OnboardingAchievementId {
    pub fn achievement(self) -> OnboardingAchievement {
        ONBOARDING_ACHIEVEMENTS
            .iter()
            .copied()
            .find(|achievement| achievement.id == self)
            .expect("all onboarding achievement ids must have definitions")
    }

    pub fn stable_id(self) -> &'static str {
        self.achievement().stable_id
    }
}

impl OnboardingEvent {
    pub fn achievement_id(self) -> OnboardingAchievementId {
        match self {
            Self::ResourceHarvested | Self::ResourceCollected => {
                OnboardingAchievementId::FirstHarvestOrCollection
            }
            Self::DroneSpawned => OnboardingAchievementId::FirstSpawn,
            Self::StructureBuilt => OnboardingAchievementId::FirstBuild,
            Self::ResourceBottleneckExplanationAvailable
            | Self::ResourceBottleneckExplanationViewed => {
                OnboardingAchievementId::ResourceBottleneckExplanation
            }
            Self::ReplayAvailable | Self::ReplayViewed => OnboardingAchievementId::Replay,
            Self::ArenaAvailable | Self::ArenaTried => OnboardingAchievementId::Arena,
        }
    }
}

impl OnboardingProgress {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, event: OnboardingEvent) -> Option<OnboardingAchievement> {
        let achievement_id = event.achievement_id();
        self.unlocked
            .insert(achievement_id)
            .then(|| achievement_id.achievement())
    }

    pub fn is_unlocked(&self, achievement_id: OnboardingAchievementId) -> bool {
        self.unlocked.contains(&achievement_id)
    }

    pub fn unlocked(&self) -> Vec<OnboardingAchievement> {
        self.unlocked
            .iter()
            .copied()
            .map(OnboardingAchievementId::achievement)
            .collect()
    }

    pub fn missing(&self) -> Vec<OnboardingAchievement> {
        ONBOARDING_ACHIEVEMENTS
            .iter()
            .copied()
            .filter(|achievement| !self.is_unlocked(achievement.id))
            .collect()
    }

    pub fn completed_count(&self) -> usize {
        self.unlocked.len()
    }

    pub fn is_complete(&self) -> bool {
        self.completed_count() == ONBOARDING_ACHIEVEMENTS.len()
    }
}

pub fn onboarding_achievements() -> &'static [OnboardingAchievement; 6] {
    &ONBOARDING_ACHIEVEMENTS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defines_six_stable_onboarding_achievements() {
        let achievements = onboarding_achievements();

        assert_eq!(achievements.len(), 6);
        assert_eq!(
            achievements
                .iter()
                .map(|achievement| achievement.stable_id)
                .collect::<Vec<_>>(),
            vec![
                "onboarding.first_harvest_or_collection",
                "onboarding.first_spawn",
                "onboarding.first_build",
                "onboarding.resource_bottleneck_explanation",
                "onboarding.replay",
                "onboarding.arena",
            ]
        );
    }

    #[test]
    fn records_first_harvest_or_collection_only_once() {
        let mut progress = OnboardingProgress::new();

        assert_eq!(
            progress.record(OnboardingEvent::ResourceHarvested),
            Some(OnboardingAchievementId::FirstHarvestOrCollection.achievement())
        );
        assert_eq!(progress.record(OnboardingEvent::ResourceCollected), None);
        assert!(progress.is_unlocked(OnboardingAchievementId::FirstHarvestOrCollection));
        assert_eq!(progress.completed_count(), 1);
    }

    #[test]
    fn records_first_spawn_and_first_build() {
        let mut progress = OnboardingProgress::new();

        assert_eq!(
            progress.record(OnboardingEvent::DroneSpawned),
            Some(OnboardingAchievementId::FirstSpawn.achievement())
        );
        assert_eq!(
            progress.record(OnboardingEvent::StructureBuilt),
            Some(OnboardingAchievementId::FirstBuild.achievement())
        );

        assert!(progress.is_unlocked(OnboardingAchievementId::FirstSpawn));
        assert!(progress.is_unlocked(OnboardingAchievementId::FirstBuild));
    }

    #[test]
    fn availability_or_view_events_unlock_bottleneck_replay_and_arena() {
        let mut progress = OnboardingProgress::new();

        progress.record(OnboardingEvent::ResourceBottleneckExplanationAvailable);
        progress.record(OnboardingEvent::ReplayViewed);
        progress.record(OnboardingEvent::ArenaAvailable);

        assert!(progress.is_unlocked(OnboardingAchievementId::ResourceBottleneckExplanation));
        assert!(progress.is_unlocked(OnboardingAchievementId::Replay));
        assert!(progress.is_unlocked(OnboardingAchievementId::Arena));
        assert_eq!(progress.record(OnboardingEvent::ArenaTried), None);
    }

    #[test]
    fn tracks_missing_and_completion() {
        let mut progress = OnboardingProgress::new();
        for event in [
            OnboardingEvent::ResourceCollected,
            OnboardingEvent::DroneSpawned,
            OnboardingEvent::StructureBuilt,
            OnboardingEvent::ResourceBottleneckExplanationViewed,
            OnboardingEvent::ReplayAvailable,
        ] {
            progress.record(event);
        }

        assert!(!progress.is_complete());
        assert_eq!(
            progress.missing(),
            vec![OnboardingAchievementId::Arena.achievement()]
        );

        progress.record(OnboardingEvent::ArenaTried);

        assert!(progress.is_complete());
        assert_eq!(progress.missing(), Vec::new());
    }
}
