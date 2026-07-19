use bevy::prelude::Resource as BevyResource;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use swarm_engine_api::ids::PlayerId;

use crate::command::Tick;
use crate::components::WorldMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LeagueTier {
    Bronze,
    Silver,
    Gold,
    Platinum,
    Diamond,
    Master,
}

impl LeagueTier {
    pub fn from_rating(rating: i32) -> Self {
        match rating {
            ..=1199 => Self::Bronze,
            1200..=1399 => Self::Silver,
            1400..=1599 => Self::Gold,
            1600..=1799 => Self::Platinum,
            1800..=2099 => Self::Diamond,
            _ => Self::Master,
        }
    }

    pub fn rank_value(self) -> u8 {
        match self {
            Self::Bronze => 0,
            Self::Silver => 1,
            Self::Gold => 2,
            Self::Platinum => 3,
            Self::Diamond => 4,
            Self::Master => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchOutcome {
    PlayerOneWin,
    PlayerTwoWin,
    Draw,
}

impl MatchOutcome {
    fn scores_per_mille(self) -> (i32, i32) {
        match self {
            Self::PlayerOneWin => (1000, 0),
            Self::PlayerTwoWin => (0, 1000),
            Self::Draw => (500, 500),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EloRating {
    pub rating: i32,
    pub k_factor: i32,
}

impl Default for EloRating {
    fn default() -> Self {
        Self {
            rating: 1500,
            k_factor: 32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlickoRating {
    pub rating: i32,
    pub deviation: u32,
    pub volatility_ppm: u32,
}

impl Default for GlickoRating {
    fn default() -> Self {
        Self {
            rating: 1500,
            deviation: 350,
            volatility_ppm: 60_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerRanking {
    pub player_id: PlayerId,
    pub elo: EloRating,
    pub glicko: GlickoRating,
    pub league: LeagueTier,
    pub season_points: i32,
    pub legacy_bonus: u32,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
}

impl PlayerRanking {
    pub fn new(player_id: PlayerId) -> Self {
        let elo = EloRating::default();
        Self {
            player_id,
            elo,
            glicko: GlickoRating::default(),
            league: LeagueTier::from_rating(elo.rating),
            season_points: 0,
            legacy_bonus: 0,
            wins: 0,
            losses: 0,
            draws: 0,
        }
    }

    fn refresh_league(&mut self) {
        self.league = LeagueTier::from_rating(self.elo.rating + self.legacy_bonus as i32);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeasonConfig {
    pub legacy_rating_divisor: i32,
    pub max_legacy_bonus: u32,
    pub carryover_numerator: u32,
    pub carryover_denominator: u32,
}

impl Default for SeasonConfig {
    fn default() -> Self {
        Self {
            legacy_rating_divisor: 50,
            max_legacy_bonus: 75,
            carryover_numerator: 1,
            carryover_denominator: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchRecord {
    pub season: u32,
    pub tick: Tick,
    pub player_one: PlayerId,
    pub player_two: PlayerId,
    pub outcome: MatchOutcome,
    pub player_one_elo_after: i32,
    pub player_two_elo_after: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaderboardEntry {
    pub rank: u32,
    pub player_id: PlayerId,
    pub league: LeagueTier,
    pub elo_rating: i32,
    pub glicko_rating: i32,
    pub glicko_deviation: u32,
    pub season_points: i32,
    pub legacy_bonus: u32,
    pub wins: u32,
    pub losses: u32,
    pub draws: u32,
}

#[derive(BevyResource, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankingState {
    pub mode: WorldMode,
    pub season: u32,
    pub season_config: SeasonConfig,
    pub players: IndexMap<PlayerId, PlayerRanking>,
    pub match_history: Vec<MatchRecord>,
}

impl Default for RankingState {
    fn default() -> Self {
        Self {
            mode: WorldMode::Default,
            season: 1,
            season_config: SeasonConfig::default(),
            players: IndexMap::new(),
            match_history: Vec::new(),
        }
    }
}

impl RankingState {
    pub fn ensure_player(&mut self, player_id: PlayerId) -> &mut PlayerRanking {
        self.players
            .entry(player_id)
            .or_insert_with(|| PlayerRanking::new(player_id))
    }

    pub fn record_match(
        &mut self,
        tick: Tick,
        player_one: PlayerId,
        player_two: PlayerId,
        outcome: MatchOutcome,
    ) -> Option<(LeaderboardEntry, LeaderboardEntry)> {
        if self.mode != WorldMode::Arena || player_one == player_two {
            return None;
        }
        let p1_before = self.ensure_player(player_one).clone();
        let p2_before = self.ensure_player(player_two).clone();
        let (p1_score, p2_score) = outcome.scores_per_mille();
        let p1_elo = update_elo(p1_before.elo, p2_before.elo.rating, p1_score);
        let p2_elo = update_elo(p2_before.elo, p1_before.elo.rating, p2_score);
        let p1_glicko = update_glicko(p1_before.glicko, p2_before.glicko, p1_score);
        let p2_glicko = update_glicko(p2_before.glicko, p1_before.glicko, p2_score);
        {
            let p1 = self.ensure_player(player_one);
            p1.elo = p1_elo;
            p1.glicko = p1_glicko;
            p1.season_points += score_points(p1_score);
            match outcome {
                MatchOutcome::PlayerOneWin => p1.wins += 1,
                MatchOutcome::PlayerTwoWin => p1.losses += 1,
                MatchOutcome::Draw => p1.draws += 1,
            }
            p1.refresh_league();
        }
        {
            let p2 = self.ensure_player(player_two);
            p2.elo = p2_elo;
            p2.glicko = p2_glicko;
            p2.season_points += score_points(p2_score);
            match outcome {
                MatchOutcome::PlayerOneWin => p2.losses += 1,
                MatchOutcome::PlayerTwoWin => p2.wins += 1,
                MatchOutcome::Draw => p2.draws += 1,
            }
            p2.refresh_league();
        }
        self.match_history.push(MatchRecord {
            season: self.season,
            tick,
            player_one,
            player_two,
            outcome,
            player_one_elo_after: p1_elo.rating,
            player_two_elo_after: p2_elo.rating,
        });
        let entries = self.leaderboard();
        Some((
            entries
                .iter()
                .find(|entry| entry.player_id == player_one)?
                .clone(),
            entries
                .iter()
                .find(|entry| entry.player_id == player_two)?
                .clone(),
        ))
    }

    pub fn leaderboard(&self) -> Vec<LeaderboardEntry> {
        let mut players = self.players.values().cloned().collect::<Vec<_>>();
        players.sort_by_key(|player| {
            (
                std::cmp::Reverse(player.league.rank_value()),
                std::cmp::Reverse(player.elo.rating + player.legacy_bonus as i32),
                std::cmp::Reverse(player.glicko.rating),
                player.glicko.deviation,
                player.player_id,
            )
        });
        players
            .into_iter()
            .enumerate()
            .map(|(index, player)| LeaderboardEntry {
                rank: index as u32 + 1,
                player_id: player.player_id,
                league: player.league,
                elo_rating: player.elo.rating,
                glicko_rating: player.glicko.rating,
                glicko_deviation: player.glicko.deviation,
                season_points: player.season_points,
                legacy_bonus: player.legacy_bonus,
                wins: player.wins,
                losses: player.losses,
                draws: player.draws,
            })
            .collect()
    }

    pub fn finalize_season(&mut self) -> Vec<LeaderboardEntry> {
        let final_board = self.leaderboard();
        let config = self.season_config;
        for entry in &final_board {
            if let Some(player) = self.players.get_mut(&entry.player_id) {
                let earned = season_legacy_bonus(player, entry.rank, config);
                let carried = player
                    .legacy_bonus
                    .saturating_mul(config.carryover_numerator)
                    / config.carryover_denominator.max(1);
                player.legacy_bonus = (carried + earned).min(config.max_legacy_bonus);
                player.season_points = 0;
                player.wins = 0;
                player.losses = 0;
                player.draws = 0;
                player.glicko.deviation = (player.glicko.deviation + 50).min(350);
                player.refresh_league();
            }
        }
        self.season += 1;
        final_board
    }
}

pub fn expected_score_per_mille(player_rating: i32, opponent_rating: i32) -> i32 {
    let diff = (player_rating - opponent_rating).clamp(-800, 800);
    (500 + diff * 400 / 800).clamp(100, 900)
}

pub fn update_elo(current: EloRating, opponent_rating: i32, score_per_mille: i32) -> EloRating {
    let expected = expected_score_per_mille(current.rating, opponent_rating);
    let delta = current.k_factor * (score_per_mille - expected) / 1000;
    EloRating {
        rating: current.rating + delta,
        ..current
    }
}

pub fn update_glicko(
    current: GlickoRating,
    opponent: GlickoRating,
    score_per_mille: i32,
) -> GlickoRating {
    let expected = expected_score_per_mille(current.rating, opponent.rating);
    let confidence = ((current.deviation + opponent.deviation) / 2).clamp(50, 350) as i32;
    let k = (16 + confidence / 8).clamp(22, 60);
    let delta = k * (score_per_mille - expected) / 1000;
    let deviation_drop = 20 + (score_per_mille - expected).unsigned_abs() / 50;
    GlickoRating {
        rating: current.rating + delta,
        deviation: current.deviation.saturating_sub(deviation_drop).max(50),
        volatility_ppm: current.volatility_ppm,
    }
}

fn score_points(score_per_mille: i32) -> i32 {
    match score_per_mille {
        1000 => 3,
        500 => 1,
        _ => 0,
    }
}

pub fn season_legacy_bonus(player: &PlayerRanking, rank: u32, season_config: SeasonConfig) -> u32 {
    let rating_bonus =
        ((player.elo.rating - 1500).max(0) / season_config.legacy_rating_divisor.max(1)) as u32;
    let win_bonus = player.wins / 5;
    let placement_bonus = match rank {
        1 => 15,
        2..=3 => 10,
        4..=10 => 5,
        _ => 0,
    };
    (rating_bonus + win_bonus + placement_bonus).min(season_config.max_legacy_bonus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn league_tier_tracks_rating_bands() {
        assert_eq!(LeagueTier::from_rating(1100), LeagueTier::Bronze);
        assert_eq!(LeagueTier::from_rating(1300), LeagueTier::Silver);
        assert_eq!(LeagueTier::from_rating(1500), LeagueTier::Gold);
        assert_eq!(LeagueTier::from_rating(1700), LeagueTier::Platinum);
        assert_eq!(LeagueTier::from_rating(1900), LeagueTier::Diamond);
        assert_eq!(LeagueTier::from_rating(2200), LeagueTier::Master);
    }

    #[test]
    fn elo_updates_winner_and_loser_symmetrically() {
        assert_eq!(update_elo(EloRating::default(), 1500, 1000).rating, 1516);
        assert_eq!(update_elo(EloRating::default(), 1500, 0).rating, 1484);
    }

    #[test]
    fn glicko_update_moves_rating_and_reduces_deviation() {
        let winner = update_glicko(GlickoRating::default(), GlickoRating::default(), 1000);
        assert!(winner.rating > 1500);
        assert!(winner.deviation < 350);
        assert_eq!(winner.volatility_ppm, 60_000);
    }

    #[test]
    fn arena_matches_feed_leaderboard_and_ignore_world_mode() {
        let mut rankings = RankingState::default();
        assert!(
            rankings
                .record_match(1, 1, 2, MatchOutcome::PlayerOneWin)
                .is_none()
        );
        rankings.mode = WorldMode::Arena;
        rankings
            .record_match(2, 1, 2, MatchOutcome::PlayerOneWin)
            .unwrap();
        rankings.record_match(3, 1, 3, MatchOutcome::Draw).unwrap();
        let board = rankings.leaderboard();
        assert_eq!(board[0].player_id, 1);
        assert_eq!(board[0].wins, 1);
        assert_eq!(board[0].draws, 1);
        assert_eq!(board[0].league, LeagueTier::Gold);
    }

    #[test]
    fn season_finalize_awards_legacy_bonus_and_resets_season_stats() {
        let mut rankings = RankingState {
            mode: WorldMode::Arena,
            ..Default::default()
        };
        for tick in 1..=8 {
            rankings
                .record_match(tick, 1, 2, MatchOutcome::PlayerOneWin)
                .unwrap();
        }
        let final_board = rankings.finalize_season();
        assert_eq!(final_board[0].player_id, 1);
        let player = rankings.players.get(&1).unwrap();
        assert!(player.legacy_bonus > 0);
        assert_eq!(player.wins, 0);
        assert_eq!(player.season_points, 0);
        assert_eq!(rankings.season, 2);
    }
}
