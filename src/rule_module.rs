use std::time::{Duration, Instant};

use bevy::prelude::*;
use serde_json::Value;

pub const DEFAULT_RHAI_AST_NODES_PER_TICK: usize = 10_000;
pub const DEFAULT_RHAI_ACTIONS_PER_TICK: usize = 100;
pub const DEFAULT_RHAI_WALL_CLOCK_PER_TICK: Duration = Duration::from_millis(100);
pub const DEFAULT_RHAI_MAX_CONSECUTIVE_OVER_BUDGET_TICKS: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RhaiExecutionBudget {
    pub ast_nodes_per_tick: usize,
    pub actions_per_tick: usize,
    pub wall_clock_per_tick: Duration,
    pub max_consecutive_over_budget_ticks: u32,
}

impl Default for RhaiExecutionBudget {
    fn default() -> Self {
        Self {
            ast_nodes_per_tick: DEFAULT_RHAI_AST_NODES_PER_TICK,
            actions_per_tick: DEFAULT_RHAI_ACTIONS_PER_TICK,
            wall_clock_per_tick: DEFAULT_RHAI_WALL_CLOCK_PER_TICK,
            max_consecutive_over_budget_ticks: DEFAULT_RHAI_MAX_CONSECUTIVE_OVER_BUDGET_TICKS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleAction {
    LogInfo(String),
    LogWarn(String),
    EmitEvent { event_type: String, data: Value },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RhaiBudgetExceeded {
    AstNodes { used: usize, limit: usize },
    Actions { used: usize, limit: usize },
    WallClock { elapsed: Duration, limit: Duration },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RhaiModuleTickReport {
    pub module_name: String,
    pub skipped: bool,
    pub disabled: bool,
    pub actions_applied: usize,
    pub actions_discarded: usize,
    pub over_budget: Vec<RhaiBudgetExceeded>,
    pub consecutive_over_budget_ticks: u32,
}

pub struct RhaiActions<'a> {
    budget: RhaiExecutionBudget,
    actions: &'a mut Vec<RuleAction>,
    discarded_actions: usize,
    action_over_budget: Option<RhaiBudgetExceeded>,
}

impl<'a> RhaiActions<'a> {
    fn new(budget: RhaiExecutionBudget, actions: &'a mut Vec<RuleAction>) -> Self {
        Self {
            budget,
            actions,
            discarded_actions: 0,
            action_over_budget: None,
        }
    }

    pub fn push_action(&mut self, action: RuleAction) -> bool {
        let attempted = self.actions.len() + self.discarded_actions + 1;
        if self.actions.len() >= self.budget.actions_per_tick {
            self.discarded_actions += 1;
            self.action_over_budget = Some(RhaiBudgetExceeded::Actions {
                used: attempted,
                limit: self.budget.actions_per_tick,
            });
            return false;
        }
        self.actions.push(action);
        true
    }

    pub fn log_info(&mut self, message: impl Into<String>) -> bool {
        self.push_action(RuleAction::LogInfo(message.into()))
    }

    pub fn log_warn(&mut self, message: impl Into<String>) -> bool {
        self.push_action(RuleAction::LogWarn(message.into()))
    }

    pub fn emit_event(&mut self, event_type: impl Into<String>, data: Value) -> bool {
        self.push_action(RuleAction::EmitEvent {
            event_type: event_type.into(),
            data,
        })
    }

    pub fn actions_applied(&self) -> usize {
        self.actions.len()
    }

    pub fn actions_discarded(&self) -> usize {
        self.discarded_actions
    }

    fn action_over_budget(&self) -> Option<RhaiBudgetExceeded> {
        self.action_over_budget
    }
}

pub trait RhaiRuleExecutor: Send + Sync + 'static {
    fn on_tick_end(&mut self, actions: &mut RhaiActions<'_>);
}

impl<F> RhaiRuleExecutor for F
where
    F: FnMut(&mut RhaiActions<'_>) + Send + Sync + 'static,
{
    fn on_tick_end(&mut self, actions: &mut RhaiActions<'_>) {
        self(actions);
    }
}

pub struct RhaiRuleModule {
    name: String,
    ast_nodes: usize,
    enabled: bool,
    consecutive_over_budget_ticks: u32,
    executor: Box<dyn RhaiRuleExecutor>,
}

impl RhaiRuleModule {
    pub fn new(name: impl Into<String>, source: &str, executor: impl RhaiRuleExecutor) -> Self {
        Self::with_ast_nodes(name, count_rhai_ast_nodes(source), executor)
    }

    pub fn with_ast_nodes(
        name: impl Into<String>,
        ast_nodes: usize,
        executor: impl RhaiRuleExecutor,
    ) -> Self {
        Self {
            name: name.into(),
            ast_nodes,
            enabled: true,
            consecutive_over_budget_ticks: 0,
            executor: Box::new(executor),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn ast_nodes(&self) -> usize {
        self.ast_nodes
    }
    pub fn set_ast_nodes(&mut self, ast_nodes: usize) {
        self.ast_nodes = ast_nodes;
    }
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
    pub fn consecutive_over_budget_ticks(&self) -> u32 {
        self.consecutive_over_budget_ticks
    }
    pub fn enable(&mut self) {
        self.enabled = true;
        self.consecutive_over_budget_ticks = 0;
    }
    pub fn disable(&mut self) {
        self.enabled = false;
    }
}

#[derive(Resource, Default)]
pub struct RhaiRuleModules {
    budget: RhaiExecutionBudget,
    modules: Vec<RhaiRuleModule>,
    last_tick_reports: Vec<RhaiModuleTickReport>,
    applied_actions: Vec<RuleAction>,
}

impl RhaiRuleModules {
    pub fn new(budget: RhaiExecutionBudget) -> Self {
        Self {
            budget,
            modules: Vec::new(),
            last_tick_reports: Vec::new(),
            applied_actions: Vec::new(),
        }
    }

    pub fn budget(&self) -> RhaiExecutionBudget {
        self.budget
    }
    pub fn set_budget(&mut self, budget: RhaiExecutionBudget) {
        self.budget = budget;
    }
    pub fn add_module(&mut self, module: RhaiRuleModule) {
        self.modules.push(module);
    }
    pub fn modules(&self) -> &[RhaiRuleModule] {
        &self.modules
    }
    pub fn modules_mut(&mut self) -> &mut [RhaiRuleModule] {
        &mut self.modules
    }
    pub fn last_tick_reports(&self) -> &[RhaiModuleTickReport] {
        &self.last_tick_reports
    }
    pub fn applied_actions(&self) -> &[RuleAction] {
        &self.applied_actions
    }
    pub fn clear_applied_actions(&mut self) {
        self.applied_actions.clear();
    }

    pub fn run_tick_end(&mut self) {
        self.last_tick_reports.clear();
        for module in &mut self.modules {
            let report = run_module_tick_end(self.budget, module, &mut self.applied_actions);
            self.last_tick_reports.push(report);
        }
    }
}

pub fn rhai_rule_module_tick_end_system(mut modules: ResMut<RhaiRuleModules>) {
    modules.run_tick_end();
}

pub fn count_rhai_ast_nodes(source: &str) -> usize {
    source
        .split(|ch: char| {
            ch.is_whitespace() || matches!(ch, '(' | ')' | '{' | '}' | '[' | ']' | ',' | ';')
        })
        .filter(|token| !token.is_empty())
        .count()
}

fn run_module_tick_end(
    budget: RhaiExecutionBudget,
    module: &mut RhaiRuleModule,
    applied_actions: &mut Vec<RuleAction>,
) -> RhaiModuleTickReport {
    let mut over_budget = Vec::new();
    if !module.enabled {
        return RhaiModuleTickReport {
            module_name: module.name.clone(),
            skipped: true,
            disabled: true,
            actions_applied: 0,
            actions_discarded: 0,
            over_budget,
            consecutive_over_budget_ticks: module.consecutive_over_budget_ticks,
        };
    }

    if module.ast_nodes > budget.ast_nodes_per_tick {
        over_budget.push(RhaiBudgetExceeded::AstNodes {
            used: module.ast_nodes,
            limit: budget.ast_nodes_per_tick,
        });
        mark_over_budget(module, budget);
        return RhaiModuleTickReport {
            module_name: module.name.clone(),
            skipped: true,
            disabled: !module.enabled,
            actions_applied: 0,
            actions_discarded: 0,
            over_budget,
            consecutive_over_budget_ticks: module.consecutive_over_budget_ticks,
        };
    }

    let mut tick_actions = Vec::new();
    let started = Instant::now();
    let mut context = RhaiActions::new(budget, &mut tick_actions);
    module.executor.on_tick_end(&mut context);
    let elapsed = started.elapsed();
    let actions_applied = context.actions_applied();
    let actions_discarded = context.actions_discarded();
    if let Some(exceeded) = context.action_over_budget() {
        over_budget.push(exceeded);
    }
    drop(context);

    let wall_clock_over_budget = elapsed > budget.wall_clock_per_tick;
    if wall_clock_over_budget {
        over_budget.push(RhaiBudgetExceeded::WallClock {
            elapsed,
            limit: budget.wall_clock_per_tick,
        });
    } else {
        applied_actions.extend(tick_actions);
    }

    if over_budget.is_empty() {
        module.consecutive_over_budget_ticks = 0;
    } else {
        mark_over_budget(module, budget);
    }

    RhaiModuleTickReport {
        module_name: module.name.clone(),
        skipped: wall_clock_over_budget,
        disabled: !module.enabled,
        actions_applied: if wall_clock_over_budget {
            0
        } else {
            actions_applied
        },
        actions_discarded,
        over_budget,
        consecutive_over_budget_ticks: module.consecutive_over_budget_ticks,
    }
}

fn mark_over_budget(module: &mut RhaiRuleModule, budget: RhaiExecutionBudget) {
    module.consecutive_over_budget_ticks = module.consecutive_over_budget_ticks.saturating_add(1);
    if module.consecutive_over_budget_ticks >= budget.max_consecutive_over_budget_ticks {
        module.disable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    #[test]
    fn default_budget_matches_design_limits() {
        let budget = RhaiExecutionBudget::default();
        assert_eq!(budget.ast_nodes_per_tick, 10_000);
        assert_eq!(budget.actions_per_tick, 100);
        assert_eq!(budget.wall_clock_per_tick, Duration::from_millis(100));
        assert_eq!(budget.max_consecutive_over_budget_ticks, 10);
    }

    #[test]
    fn ast_over_budget_skips_hook_and_disables_after_ten_ticks() {
        let mut modules = RhaiRuleModules::default();
        modules.add_module(RhaiRuleModule::with_ast_nodes(
            "large",
            10_001,
            |_: &mut RhaiActions<'_>| panic!("over-budget AST module should be skipped"),
        ));
        for tick in 1..=10 {
            modules.run_tick_end();
            let report = &modules.last_tick_reports()[0];
            assert!(report.skipped);
            assert_eq!(report.consecutive_over_budget_ticks, tick);
        }
        assert!(!modules.modules()[0].is_enabled());
    }

    #[test]
    fn action_budget_discards_excess_actions() {
        let mut modules = RhaiRuleModules::default();
        modules.add_module(RhaiRuleModule::with_ast_nodes(
            "chatty",
            1,
            |actions: &mut RhaiActions<'_>| {
                for i in 0..105 {
                    actions.log_info(format!("message {i}"));
                }
            },
        ));
        modules.run_tick_end();
        let report = &modules.last_tick_reports()[0];
        assert_eq!(report.actions_applied, 100);
        assert_eq!(report.actions_discarded, 5);
        assert_eq!(modules.applied_actions().len(), 100);
        assert!(matches!(
            report.over_budget[0],
            RhaiBudgetExceeded::Actions {
                used: 105,
                limit: 100
            }
        ));
    }

    #[test]
    fn wall_clock_over_budget_rolls_back_actions() {
        let mut modules = RhaiRuleModules::new(RhaiExecutionBudget {
            wall_clock_per_tick: Duration::from_millis(1),
            ..RhaiExecutionBudget::default()
        });
        modules.add_module(RhaiRuleModule::with_ast_nodes(
            "slow",
            1,
            |actions: &mut RhaiActions<'_>| {
                actions.log_info("before sleep");
                thread::sleep(Duration::from_millis(5));
            },
        ));
        modules.run_tick_end();
        let report = &modules.last_tick_reports()[0];
        assert!(report.skipped);
        assert_eq!(report.actions_applied, 0);
        assert!(modules.applied_actions().is_empty());
        assert!(matches!(
            report.over_budget[0],
            RhaiBudgetExceeded::WallClock { .. }
        ));
    }

    #[test]
    fn successful_tick_resets_consecutive_over_budget_count() {
        let mut module =
            RhaiRuleModule::with_ast_nodes("recovering", 10_001, |_: &mut RhaiActions<'_>| {});
        let mut actions = Vec::new();
        run_module_tick_end(RhaiExecutionBudget::default(), &mut module, &mut actions);
        assert_eq!(module.consecutive_over_budget_ticks(), 1);
        module.set_ast_nodes(1);
        run_module_tick_end(RhaiExecutionBudget::default(), &mut module, &mut actions);
        assert_eq!(module.consecutive_over_budget_ticks(), 0);
    }
}
