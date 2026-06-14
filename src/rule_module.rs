use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::prelude::*;
use rhai::{Engine, EvalAltResult, Scope};
use serde_json::Value;

use crate::components::{Drone, Owner, PlayerId, Resource, Structure};

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
    DeductResource {
        player_id: PlayerId,
        resource: String,
        amount: u32,
    },
    AwardResource {
        player_id: PlayerId,
        resource: String,
        amount: u32,
    },
    EmitEvent {
        event_type: String,
        data: Value,
    },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RhaiRuleHook {
    Init,
    TickStart,
    TickEnd,
}

impl RhaiRuleHook {
    fn filename(self) -> &'static str {
        match self {
            Self::Init => "init.rhai",
            Self::TickStart => "tick_start.rhai",
            Self::TickEnd => "tick_end.rhai",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RhaiScriptModule {
    pub name: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RhaiScriptError {
    pub module_name: String,
    pub hook: RhaiRuleHook,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RhaiHookReport {
    pub actions_applied: usize,
    pub actions_skipped: usize,
    pub events_emitted: usize,
    pub errors: Vec<RhaiScriptError>,
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

    pub fn deduct_resource(
        &mut self,
        player_id: PlayerId,
        resource: impl Into<String>,
        amount: u32,
    ) -> bool {
        self.push_action(RuleAction::DeductResource {
            player_id,
            resource: resource.into(),
            amount,
        })
    }

    pub fn award_resource(
        &mut self,
        player_id: PlayerId,
        resource: impl Into<String>,
        amount: u32,
    ) -> bool {
        self.push_action(RuleAction::AwardResource {
            player_id,
            resource: resource.into(),
            amount,
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
    script_modules: Vec<RhaiScriptModule>,
    last_hook_report: RhaiHookReport,
}

impl RhaiRuleModules {
    pub fn new(budget: RhaiExecutionBudget) -> Self {
        Self {
            budget,
            modules: Vec::new(),
            last_tick_reports: Vec::new(),
            applied_actions: Vec::new(),
            script_modules: Vec::new(),
            last_hook_report: RhaiHookReport::default(),
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

    pub fn add_script_module_dir(&mut self, dir: impl AsRef<Path>) {
        let root = dir.as_ref().to_path_buf();
        let name = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rule-module")
            .to_string();
        self.script_modules.push(RhaiScriptModule { name, root });
    }

    pub fn script_modules(&self) -> &[RhaiScriptModule] {
        &self.script_modules
    }

    pub fn last_hook_report(&self) -> &RhaiHookReport {
        &self.last_hook_report
    }

    pub fn run_hook(&mut self, world: &mut World, hook: RhaiRuleHook) -> RhaiHookReport {
        let mut report = RhaiHookReport::default();
        for module in self.script_modules.clone() {
            let script_path = module.root.join(hook.filename());
            if !script_path.exists() {
                continue;
            }
            match execute_rhai_script(&script_path, self.budget) {
                Ok(actions) => {
                    self.applied_actions.extend(actions.clone());
                    apply_rule_actions(world, &actions, &mut report);
                }
                Err(error) => report.errors.push(RhaiScriptError {
                    module_name: module.name,
                    hook,
                    message: error.to_string(),
                }),
            }
        }
        self.last_hook_report = report.clone();
        report
    }

    pub fn run_tick_end(&mut self) {
        self.last_tick_reports.clear();
        for module in &mut self.modules {
            let report = run_module_tick_end(self.budget, module, &mut self.applied_actions);
            self.last_tick_reports.push(report);
        }
    }
}

pub fn rhai_rule_module_tick_end_system(world: &mut World) {
    world.resource_scope(|world, mut modules: Mut<RhaiRuleModules>| {
        let start = modules.applied_actions.len();
        modules.run_tick_end();
        let actions = modules.applied_actions[start..].to_vec();
        let mut report = RhaiHookReport::default();
        apply_rule_actions(world, &actions, &mut report);
        let script_report = modules.run_hook(world, RhaiRuleHook::TickEnd);
        report.actions_applied += script_report.actions_applied;
        report.actions_skipped += script_report.actions_skipped;
        report.events_emitted += script_report.events_emitted;
        report.errors.extend(script_report.errors);
        modules.last_hook_report = report;
    });
}

pub fn run_init_scripts(world: &mut World) -> RhaiHookReport {
    run_script_hook(world, RhaiRuleHook::Init)
}

pub fn run_tick_start_scripts(world: &mut World) -> RhaiHookReport {
    run_script_hook(world, RhaiRuleHook::TickStart)
}

pub fn run_tick_end_scripts(world: &mut World) -> RhaiHookReport {
    run_script_hook(world, RhaiRuleHook::TickEnd)
}

fn run_script_hook(world: &mut World, hook: RhaiRuleHook) -> RhaiHookReport {
    world.resource_scope(|world, mut modules: Mut<RhaiRuleModules>| modules.run_hook(world, hook))
}

#[derive(Clone)]
struct RhaiActionBuffer {
    actions: Arc<Mutex<Vec<RuleAction>>>,
}

fn execute_rhai_script(
    script_path: &Path,
    budget: RhaiExecutionBudget,
) -> Result<Vec<RuleAction>, Box<EvalAltResult>> {
    let script = fs::read_to_string(script_path).map_err(|error| {
        EvalAltResult::ErrorSystem(
            format!("failed to read {}", script_path.display()),
            Box::new(error),
        )
    })?;
    let buffered = Arc::new(Mutex::new(Vec::new()));
    let actions = RhaiActionBuffer {
        actions: Arc::clone(&buffered),
    };
    let mut engine = Engine::new();
    engine.set_max_expr_depths(32, 32);
    engine.set_max_operations(budget.ast_nodes_per_tick as u64);
    engine.register_type::<RhaiActionBuffer>();
    engine.register_fn("deduct_resource", rhai_deduct_resource);
    engine.register_fn("award_resource", rhai_award_resource);
    engine.register_fn("emit_event", rhai_emit_event);

    let ast = engine.compile(&script)?;
    let mut scope = Scope::new();
    scope.push("actions", actions);
    engine.run_ast_with_scope(&mut scope, &ast)?;
    let actions = buffered
        .lock()
        .expect("rhai action buffer lock should not be poisoned")
        .clone();
    Ok(actions)
}

fn rhai_deduct_resource(
    actions: &mut RhaiActionBuffer,
    player_id: i64,
    resource: &str,
    amount: i64,
) {
    if let (Ok(player_id), Ok(amount)) = (u32::try_from(player_id), u32::try_from(amount)) {
        actions
            .actions
            .lock()
            .expect("rhai action buffer lock should not be poisoned")
            .push(RuleAction::DeductResource {
                player_id,
                resource: resource.to_string(),
                amount,
            });
    }
}

fn rhai_award_resource(
    actions: &mut RhaiActionBuffer,
    player_id: i64,
    resource: &str,
    amount: i64,
) {
    if let (Ok(player_id), Ok(amount)) = (u32::try_from(player_id), u32::try_from(amount)) {
        actions
            .actions
            .lock()
            .expect("rhai action buffer lock should not be poisoned")
            .push(RuleAction::AwardResource {
                player_id,
                resource: resource.to_string(),
                amount,
            });
    }
}

fn rhai_emit_event(actions: &mut RhaiActionBuffer, event_type: &str, data: &str) {
    actions
        .actions
        .lock()
        .expect("rhai action buffer lock should not be poisoned")
        .push(RuleAction::EmitEvent {
            event_type: event_type.to_string(),
            data: Value::String(data.to_string()),
        });
}

fn apply_rule_actions(world: &mut World, actions: &[RuleAction], report: &mut RhaiHookReport) {
    for action in actions {
        match action {
            RuleAction::DeductResource {
                player_id,
                resource,
                amount,
            } => {
                if deduct_player_resource(world, *player_id, resource, *amount) {
                    report.actions_applied += 1;
                } else {
                    report.actions_skipped += 1;
                }
            }
            RuleAction::AwardResource {
                player_id,
                resource,
                amount,
            } => {
                if award_player_resource(world, *player_id, resource, *amount) {
                    report.actions_applied += 1;
                } else {
                    report.actions_skipped += 1;
                }
            }
            RuleAction::EmitEvent { event_type, data } => {
                report.events_emitted += 1;
                report.actions_applied += 1;
                let _ = (event_type, data);
            }
            RuleAction::LogInfo(_) | RuleAction::LogWarn(_) => {
                report.actions_applied += 1;
            }
        }
    }
}

fn deduct_player_resource(
    world: &mut World,
    player_id: PlayerId,
    resource: &str,
    amount: u32,
) -> bool {
    if player_resource_total(world, player_id, resource) < amount {
        return false;
    }
    let mut remaining = amount;
    let mut drones = world.query::<(&mut Drone, Option<&Owner>)>();
    for (mut drone, owner) in drones.iter_mut(world) {
        if owner.map(|owner| owner.0).unwrap_or(drone.owner) != player_id {
            continue;
        }
        let take = (*drone.carry.get(resource).unwrap_or(&0)).min(remaining);
        if take > 0 {
            *drone.carry.entry(resource.to_string()).or_default() -= take;
            remaining -= take;
        }
        if remaining == 0 {
            return true;
        }
    }
    if resource == "Energy" {
        let mut structures = world.query::<&mut Structure>();
        for mut structure in structures.iter_mut(world) {
            if structure.owner != Some(player_id) {
                continue;
            }
            let available = structure.energy.unwrap_or(0);
            let take = available.min(remaining);
            if take > 0 {
                structure.energy = Some(available - take);
                remaining -= take;
            }
            if remaining == 0 {
                return true;
            }
        }
    }
    let mut drops = world.query::<(&mut Resource, Option<&Owner>)>();
    for (mut dropped, owner) in drops.iter_mut(world) {
        if owner.map(|owner| owner.0) != Some(player_id) {
            continue;
        }
        let take = (*dropped.amounts.get(resource).unwrap_or(&0)).min(remaining);
        if take > 0 {
            *dropped.amounts.entry(resource.to_string()).or_default() -= take;
            remaining -= take;
        }
        if remaining == 0 {
            return true;
        }
    }
    remaining == 0
}

fn award_player_resource(
    world: &mut World,
    player_id: PlayerId,
    resource: &str,
    amount: u32,
) -> bool {
    if amount == 0 {
        return true;
    }
    let mut drones = world.query::<(&mut Drone, Option<&Owner>)>();
    for (mut drone, owner) in drones.iter_mut(world) {
        if owner.map(|owner| owner.0).unwrap_or(drone.owner) != player_id {
            continue;
        }
        let used = drone.carry.values().copied().sum::<u32>();
        if drone.carry_capacity.saturating_sub(used) >= amount {
            *drone.carry.entry(resource.to_string()).or_default() += amount;
            return true;
        }
    }
    if resource == "Energy" {
        let mut structures = world.query::<&mut Structure>();
        for mut structure in structures.iter_mut(world) {
            if structure.owner != Some(player_id) {
                continue;
            }
            if let Some(capacity) = structure.energy_capacity {
                let current = structure.energy.unwrap_or(0);
                if capacity.saturating_sub(current) >= amount {
                    structure.energy = Some(current + amount);
                    return true;
                }
            }
        }
    }
    false
}

fn player_resource_total(world: &mut World, player_id: PlayerId, resource: &str) -> u32 {
    let mut total = 0_u32;
    let mut drones = world.query::<(&Drone, Option<&Owner>)>();
    for (drone, owner) in drones.iter(world) {
        if owner.map(|owner| owner.0).unwrap_or(drone.owner) == player_id {
            total = total.saturating_add(*drone.carry.get(resource).unwrap_or(&0));
        }
    }
    if resource == "Energy" {
        let mut structures = world.query::<&Structure>();
        for structure in structures.iter(world) {
            if structure.owner == Some(player_id) {
                total = total.saturating_add(structure.energy.unwrap_or(0));
            }
        }
    }
    let mut drops = world.query::<(&Resource, Option<&Owner>)>();
    for (dropped, owner) in drops.iter(world) {
        if owner.map(|owner| owner.0) == Some(player_id) {
            total = total.saturating_add(*dropped.amounts.get(resource).unwrap_or(&0));
        }
    }
    total
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

    #[test]
    fn script_hooks_load_missing_scripts_and_apply_actions_transactionally() {
        let temp = tempfile::tempdir().expect("tempdir");
        let module_dir = temp.path().join("script-mod");
        fs::create_dir(&module_dir).expect("module dir");
        fs::write(
            module_dir.join("init.rhai"),
            r#"actions.emit_event("init", "loaded");"#,
        )
        .unwrap();
        fs::write(
            module_dir.join("tick_start.rhai"),
            r#"actions.deduct_resource(1, "Energy", 10); actions.emit_event("hook", "start");"#,
        )
        .unwrap();
        let mut world = crate::create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![crate::BodyPart::Carry]);
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .carry
            .insert("Energy".to_string(), 20);
        world
            .app
            .world_mut()
            .resource_mut::<RhaiRuleModules>()
            .add_script_module_dir(&module_dir);

        let init = run_init_scripts(world.app.world_mut());
        let start = run_tick_start_scripts(world.app.world_mut());
        let end = run_tick_end_scripts(world.app.world_mut());

        assert!(init.errors.is_empty());
        assert_eq!(init.events_emitted, 1);
        assert!(start.errors.is_empty());
        assert_eq!(start.actions_applied, 2);
        assert_eq!(
            world
                .app
                .world()
                .entity(drone)
                .get::<Drone>()
                .unwrap()
                .carry
                .get("Energy"),
            Some(&10)
        );
        assert!(end.errors.is_empty());
        assert_eq!(end.actions_applied, 0);
    }

    #[test]
    fn script_error_discards_buffer_and_action_failure_skips_only_that_action() {
        let temp = tempfile::tempdir().expect("tempdir");
        let module_dir = temp.path().join("script-mod");
        fs::create_dir(&module_dir).expect("module dir");
        fs::write(
            module_dir.join("tick_start.rhai"),
            r#"actions.deduct_resource(1, "Energy", 999); actions.emit_event("hook", "kept");"#,
        )
        .unwrap();
        fs::write(
            module_dir.join("tick_end.rhai"),
            r#"actions.award_resource(1, "Energy", 5); throw "boom";"#,
        )
        .unwrap();
        let mut world = crate::create_world();
        let drone = world.spawn_drone(1, 10, 10, vec![crate::BodyPart::Carry]);
        world
            .app
            .world_mut()
            .entity_mut(drone)
            .get_mut::<Drone>()
            .unwrap()
            .carry
            .insert("Energy".to_string(), 5);
        world
            .app
            .world_mut()
            .resource_mut::<RhaiRuleModules>()
            .add_script_module_dir(&module_dir);

        let start = run_tick_start_scripts(world.app.world_mut());
        let end = run_tick_end_scripts(world.app.world_mut());

        assert_eq!(start.actions_skipped, 1);
        assert_eq!(start.events_emitted, 1);
        assert_eq!(end.errors.len(), 1);
        assert_eq!(
            world
                .app
                .world()
                .entity(drone)
                .get::<Drone>()
                .unwrap()
                .carry
                .get("Energy"),
            Some(&5)
        );
    }
}
