use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::command::{
    object_id, validate_command, CommandAuth, CommandIntent, CommandSource, ObjectId, RawCommand,
    RejectionReason, Tick,
};
use crate::components::*;
use crate::hot_cache::{read_through_dragonfly, SnapshotKey};
use crate::resources::{PendingGlobalTransfers, PlayerGlobalStorage, PlayerLocalStorage};
use crate::visibility::{
    is_position_visible_to, visible_entity_ids, visible_positions, VISIBILITY_RADIUS,
};
use crate::world::SwarmWorld;

const MAX_WASM_BYTES: usize = 5 * 1024 * 1024;
const CERTIFICATE_TTL_SECONDS: u64 = 24 * 60 * 60;
const WEB_ACCESS_TOKEN_TTL_SECONDS: u64 = 15 * 60;
const WEB_REFRESH_TOKEN_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;
const CERTIFICATE_AUDIENCE: &str = "swarm-wasm-deploy";
const WEB_TOKEN_AUDIENCE: &str = "swarm-web";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpContext {
    pub player_id: PlayerId,
    pub tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpError {
    pub code: i32,
    pub message: String,
}

impl McpError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("unknown MCP tool: {method}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<McpError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleWorldSnapshot {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub room_id: u32,
    pub visibility_radius: i32,
    pub visible_tiles: Vec<VisibleTile>,
    pub entities: Vec<VisibleEntity>,
    pub local_storage: BTreeMap<String, u32>,
    pub global_storage: BTreeMap<String, u32>,
    pub pending_global_transfers: Vec<VisiblePendingGlobalTransfer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisiblePendingGlobalTransfer {
    pub player_id: PlayerId,
    pub direction: String,
    pub resource: String,
    pub amount: u32,
    pub deliver_amount: u32,
    pub remaining_ticks: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VisibleTile {
    pub x: i32,
    pub y: i32,
    pub room_id: u32,
    pub terrain: TerrainType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum VisibleEntity {
    Drone(VisibleDrone),
    Structure(VisibleStructure),
    Source(VisibleSource),
    Resource(VisibleResource),
    Controller(VisibleController),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisiblePosition {
    pub x: i32,
    pub y: i32,
    pub room_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleDrone {
    pub id: ObjectId,
    pub owner: PlayerId,
    pub position: VisiblePosition,
    pub body: Vec<BodyPart>,
    pub carry: BTreeMap<String, u32>,
    pub carry_capacity: u32,
    pub fatigue: u32,
    pub hits: u32,
    pub hits_max: u32,
    pub spawning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleStructure {
    pub id: ObjectId,
    pub structure_type: StructureType,
    pub owner: Option<PlayerId>,
    pub position: VisiblePosition,
    pub hits: u32,
    pub hits_max: u32,
    pub energy: Option<u32>,
    pub energy_capacity: Option<u32>,
    pub cooldown: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleSource {
    pub id: ObjectId,
    pub position: VisiblePosition,
    pub produces: BTreeMap<String, u32>,
    pub capacity: u32,
    pub ticks_to_regeneration: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleResource {
    pub id: ObjectId,
    pub position: VisiblePosition,
    pub amounts: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisibleController {
    pub id: ObjectId,
    pub owner: Option<PlayerId>,
    pub position: VisiblePosition,
    pub level: u8,
    pub progress: u32,
    pub progress_total: u32,
    pub safe_mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldRules {
    pub ruleset: String,
    pub room_size: i32,
    pub visibility_radius: i32,
    pub max_wasm_bytes: usize,
    pub active_mods: Vec<WorldRuleMod>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldRuleMod {
    pub id: String,
    pub version: String,
    pub description: String,
    pub config: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth2LoginParams {
    pub provider: String,
    pub subject: String,
    pub access_token: String,
    pub client_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth2CallbackParams {
    pub provider: String,
    pub code: String,
    pub state: String,
    pub redirect_uri: String,
    pub client_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRefreshParams {
    pub refresh_token: String,
    pub client_public_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeAuthParams {
    pub refresh_token: Option<String>,
    pub certificate: Option<PlayerCertificate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerCertificatePayload {
    pub audience: String,
    pub player_id: PlayerId,
    pub provider: String,
    pub subject: String,
    pub client_public_key: String,
    pub issued_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerCertificate {
    pub payload: PlayerCertificatePayload,
    pub issuer_public_key: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuth2LoginResult {
    pub player_id: PlayerId,
    pub session: WebAuthSession,
    pub certificate: PlayerCertificate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebAuthSession {
    pub player_id: PlayerId,
    pub provider: String,
    pub subject: String,
    pub audience: String,
    pub access_token: String,
    pub access_token_expires_at: u64,
    pub refresh_token: String,
    pub refresh_token_expires_at: u64,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRefreshResult {
    pub player_id: PlayerId,
    pub session: WebAuthSession,
    pub certificate: PlayerCertificate,
    pub renew_after_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevokeAuthResult {
    pub revoked_session: bool,
    pub revoked_certificate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployParams {
    pub wasm_bytes: String,
    pub certificate: PlayerCertificate,
    pub wasm_signature: String,
    pub language: String,
    pub version_tag: String,
    pub room_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployResult {
    pub module_id: String,
    pub status: String,
    pub deployed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredModule {
    pub module_id: String,
    pub player_id: PlayerId,
    pub room_id: RoomId,
    pub wasm_bytes: Vec<u8>,
    pub wasm_hash: String,
    pub certificate: PlayerCertificate,
    pub wasm_signature: String,
    pub language: String,
    pub version_tag: String,
    pub deployed_at: String,
    pub load_after_tick: Tick,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableActionsResult {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub wasm_actions: Vec<String>,
    pub mcp_tools: Vec<ToolInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickExplanation {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub state_checksum: u64,
    pub visible_entity_count: usize,
    pub visible_tile_count: usize,
    pub accepted_commands: usize,
    pub rejected_commands: usize,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileResult {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub deployed_modules: usize,
    pub pending_modules: usize,
    pub owned_visible_drones: usize,
    pub owned_visible_structures: usize,
    pub available_mcp_tools: usize,
    pub direct_gameplay_tools_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DryRunCommandsParams {
    pub commands: Vec<CommandIntent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunCommandResult {
    pub sequence: u32,
    pub command: RawCommand,
    pub accepted: bool,
    pub rejection: Option<RejectionReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunCommandsResult {
    pub tick: Tick,
    pub player_id: PlayerId,
    pub commands: Vec<DryRunCommandResult>,
    pub state_checksum_before: u64,
    pub state_checksum_after: u64,
    pub mutated_world: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocsParams {
    #[serde(default = "default_docs_topic")]
    pub topic: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocsSection {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocsResult {
    pub topic: String,
    pub sections: Vec<DocsSection>,
}

#[derive(Debug, Default)]
pub struct McpServer {
    modules: Vec<StoredModule>,
    issuer: CertificateIssuer,
    sessions: BTreeMap<String, WebAuthSession>,
    revoked_certificates: BTreeSet<String>,
    now_seconds: Option<u64>,
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            modules: Vec::new(),
            issuer: CertificateIssuer::new(),
            sessions: BTreeMap::new(),
            revoked_certificates: BTreeSet::new(),
            now_seconds: None,
        }
    }

    pub fn with_issuer_for_tests(issuer: SigningKey, now_seconds: u64) -> Self {
        Self {
            modules: Vec::new(),
            issuer: CertificateIssuer {
                signing_key: issuer,
            },
            sessions: BTreeMap::new(),
            revoked_certificates: BTreeSet::new(),
            now_seconds: Some(now_seconds),
        }
    }

    pub fn handle_json_rpc(
        &mut self,
        world: &mut SwarmWorld,
        context: McpContext,
        request: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let id = request.id.clone();
        if request.jsonrpc != "2.0" {
            return error_response(id, McpError::invalid_params("jsonrpc must be 2.0"));
        }

        match self.call_tool(world, context, &request.method, request.params) {
            Ok(result) => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => error_response(id, error),
        }
    }

    pub fn call_tool(
        &mut self,
        world: &mut SwarmWorld,
        context: McpContext,
        tool: &str,
        params: Value,
    ) -> Result<Value, McpError> {
        match tool {
            "swarm_get_snapshot" => serde_json::to_value(swarm_get_snapshot(world, context))
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_get_world_rules" => serde_json::to_value(swarm_get_world_rules())
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_get_available_actions" => {
                serde_json::to_value(swarm_get_available_actions(context))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_explain_last_tick" => {
                serde_json::to_value(swarm_explain_last_tick(world, context))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_profile" => serde_json::to_value(self.swarm_profile(world, context))
                .map_err(|error| McpError::invalid_params(error.to_string())),
            "swarm_dry_run_commands" => {
                let params: DryRunCommandsParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(swarm_dry_run_commands(world, context, params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_get_docs" => {
                let params: DocsParams = if params.is_null() {
                    DocsParams {
                        topic: default_docs_topic(),
                    }
                } else {
                    serde_json::from_value(params)
                        .map_err(|error| McpError::invalid_params(error.to_string()))?
                };
                serde_json::to_value(swarm_get_docs(params))
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_oauth2_callback" => {
                let params: OAuth2CallbackParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_oauth2_callback(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_token_refresh" => {
                let params: TokenRefreshParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_token_refresh(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_auth_revoke" => {
                let params: RevokeAuthParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_auth_revoke(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_oauth2_login" => {
                let params: OAuth2LoginParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_oauth2_login(params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            "swarm_deploy" => {
                let params: DeployParams = serde_json::from_value(params)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                serde_json::to_value(self.swarm_deploy(world, context, params)?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))
            }
            method => Err(McpError::method_not_found(method)),
        }
    }

    pub fn swarm_oauth2_login(
        &mut self,
        params: OAuth2LoginParams,
    ) -> Result<OAuth2LoginResult, McpError> {
        if params.provider.trim().is_empty() {
            return Err(McpError::invalid_params("provider is required"));
        }
        if params.subject.trim().is_empty() {
            return Err(McpError::invalid_params("subject is required"));
        }
        if params.access_token.trim().is_empty() {
            return Err(McpError::invalid_params("access_token is required"));
        }
        decode_ed25519_public_key(&params.client_public_key, "client_public_key")?;
        self.issue_login(
            params.provider,
            params.subject,
            params.access_token,
            params.client_public_key,
        )
    }

    pub fn swarm_oauth2_callback(
        &mut self,
        params: OAuth2CallbackParams,
    ) -> Result<OAuth2LoginResult, McpError> {
        let provider = params.provider.trim().to_ascii_lowercase();
        if provider != "github" && provider != "google" {
            return Err(McpError::invalid_params(
                "provider must be github or google",
            ));
        }
        if params.code.trim().is_empty() {
            return Err(McpError::invalid_params("code is required"));
        }
        if params.state.trim().is_empty() {
            return Err(McpError::invalid_params("state is required"));
        }
        if params.redirect_uri.trim().is_empty() {
            return Err(McpError::invalid_params("redirect_uri is required"));
        }
        decode_ed25519_public_key(&params.client_public_key, "client_public_key")?;
        let subject = format!(
            "{}:{}",
            provider,
            blake3::hash(params.code.as_bytes()).to_hex()
        );
        self.issue_login(provider, subject, params.code, params.client_public_key)
    }

    pub fn swarm_token_refresh(
        &mut self,
        params: TokenRefreshParams,
    ) -> Result<TokenRefreshResult, McpError> {
        if params.refresh_token.trim().is_empty() {
            return Err(McpError::invalid_params("refresh_token is required"));
        }
        decode_ed25519_public_key(&params.client_public_key, "client_public_key")?;
        let now = self.now_seconds();
        let stored = self
            .sessions
            .get(&params.refresh_token)
            .cloned()
            .ok_or_else(|| McpError::invalid_params("refresh_token is invalid"))?;
        if stored.refresh_token_expires_at <= now {
            self.sessions.remove(&params.refresh_token);
            return Err(McpError::invalid_params("refresh_token is expired"));
        }
        let mut session = stored.clone();
        session.access_token = opaque_web_token(
            "web_access",
            &stored.provider,
            &stored.subject,
            &params.refresh_token,
            now,
        );
        session.access_token_expires_at = now + WEB_ACCESS_TOKEN_TTL_SECONDS;
        self.sessions
            .insert(session.refresh_token.clone(), session.clone());
        let certificate = self.issue_certificate(
            session.player_id,
            session.provider.clone(),
            session.subject.clone(),
            params.client_public_key,
        )?;
        Ok(TokenRefreshResult {
            player_id: session.player_id,
            session,
            certificate,
            renew_after_seconds: CERTIFICATE_TTL_SECONDS,
        })
    }

    pub fn swarm_auth_revoke(
        &mut self,
        params: RevokeAuthParams,
    ) -> Result<RevokeAuthResult, McpError> {
        let revoked_session = params
            .refresh_token
            .as_deref()
            .map(|t| self.sessions.remove(t).is_some())
            .unwrap_or(false);
        let revoked_certificate = if let Some(certificate) = params.certificate {
            self.issuer.verify(&certificate)?;
            self.revoked_certificates.insert(certificate.signature)
        } else {
            false
        };
        Ok(RevokeAuthResult {
            revoked_session,
            revoked_certificate,
        })
    }

    fn issue_login(
        &mut self,
        provider: String,
        subject: String,
        seed: String,
        client_public_key: String,
    ) -> Result<OAuth2LoginResult, McpError> {
        let player_id = oauth_player_id(&provider, &subject);
        let now = self.now_seconds();
        let session = WebAuthSession {
            player_id,
            provider: provider.clone(),
            subject: subject.clone(),
            audience: WEB_TOKEN_AUDIENCE.to_string(),
            access_token: opaque_web_token("web_access", &provider, &subject, &seed, now),
            access_token_expires_at: now + WEB_ACCESS_TOKEN_TTL_SECONDS,
            refresh_token: opaque_web_token("web_refresh", &provider, &subject, &seed, now),
            refresh_token_expires_at: now + WEB_REFRESH_TOKEN_TTL_SECONDS,
            scopes: vec!["web".to_string(), "mcp:deploy".to_string()],
        };
        self.sessions
            .insert(session.refresh_token.clone(), session.clone());
        let certificate =
            self.issue_certificate(player_id, provider, subject, client_public_key)?;
        Ok(OAuth2LoginResult {
            player_id,
            session,
            certificate,
        })
    }

    fn issue_certificate(
        &self,
        player_id: PlayerId,
        provider: String,
        subject: String,
        client_public_key: String,
    ) -> Result<PlayerCertificate, McpError> {
        let issued_at = self.now_seconds();
        self.issuer.issue(PlayerCertificatePayload {
            audience: CERTIFICATE_AUDIENCE.to_string(),
            player_id,
            provider,
            subject,
            client_public_key,
            issued_at,
            expires_at: issued_at + CERTIFICATE_TTL_SECONDS,
        })
    }

    pub fn swarm_deploy(
        &mut self,
        world: &SwarmWorld,
        context: McpContext,
        params: DeployParams,
    ) -> Result<DeployResult, McpError> {
        if params.language.trim().is_empty() {
            return Err(McpError::invalid_params("language is required"));
        }
        if params.version_tag.trim().is_empty() {
            return Err(McpError::invalid_params("version_tag is required"));
        }

        let room_id = RoomId(params.room_id);
        if !world
            .app
            .world()
            .resource::<RoomTerrains>()
            .0
            .contains_key(&room_id)
        {
            return Err(McpError::invalid_params("room_id does not exist"));
        }

        let wasm_bytes = decode_base64(&params.wasm_bytes)?;
        if wasm_bytes.is_empty() {
            return Err(McpError::invalid_params("wasm_bytes is empty"));
        }
        if wasm_bytes.len() > MAX_WASM_BYTES {
            return Err(McpError::invalid_params("wasm module exceeds max size"));
        }
        if !wasm_bytes.starts_with(b"\0asm") {
            return Err(McpError::invalid_params("wasm_bytes must be a wasm module"));
        }
        self.verify_certificate_for_player(&params.certificate, context.player_id)?;
        let wasm_hash = blake3::hash(&wasm_bytes);
        verify_wasm_signature(
            &params.certificate,
            wasm_hash.as_bytes(),
            &params.wasm_signature,
        )?;

        let module_id = format!(
            "mod_{}_{}_{}",
            context.player_id,
            params.room_id,
            self.modules.len() + 1
        );
        let deployed_at = unix_timestamp_string();
        self.modules.push(StoredModule {
            module_id: module_id.clone(),
            player_id: context.player_id,
            room_id,
            wasm_bytes,
            wasm_hash: wasm_hash.to_hex().to_string(),
            certificate: params.certificate,
            wasm_signature: params.wasm_signature,
            language: params.language,
            version_tag: params.version_tag,
            deployed_at: deployed_at.clone(),
            load_after_tick: context.tick + 1,
        });

        Ok(DeployResult {
            module_id,
            status: "pending_next_tick".to_string(),
            deployed_at,
        })
    }

    pub fn modules(&self) -> &[StoredModule] {
        &self.modules
    }

    fn verify_certificate_for_player(
        &self,
        certificate: &PlayerCertificate,
        player_id: PlayerId,
    ) -> Result<(), McpError> {
        if certificate.payload.player_id != player_id {
            return Err(McpError::invalid_params(
                "certificate player_id does not match context",
            ));
        }
        if certificate.payload.audience != CERTIFICATE_AUDIENCE {
            return Err(McpError::invalid_params("certificate audience is invalid"));
        }
        if certificate.payload.expires_at <= self.now_seconds() {
            return Err(McpError::invalid_params("certificate is expired"));
        }
        if self.revoked_certificates.contains(&certificate.signature) {
            return Err(McpError::invalid_params("certificate is revoked"));
        }
        self.issuer.verify(certificate)
    }

    fn now_seconds(&self) -> u64 {
        self.now_seconds.unwrap_or_else(unix_timestamp_seconds)
    }

    pub fn swarm_profile(&self, world: &mut SwarmWorld, context: McpContext) -> ProfileResult {
        swarm_profile(world, context, self.modules.len())
    }
}

#[derive(Debug, Clone)]
struct CertificateIssuer {
    signing_key: SigningKey,
}

impl Default for CertificateIssuer {
    fn default() -> Self {
        Self::new()
    }
}

impl CertificateIssuer {
    fn new() -> Self {
        let mut seed = [0_u8; 32];
        getrandom::getrandom(&mut seed).expect("OS randomness is required for certificate issuer");
        Self {
            signing_key: SigningKey::from_bytes(&seed),
        }
    }

    fn issue(&self, payload: PlayerCertificatePayload) -> Result<PlayerCertificate, McpError> {
        let payload_bytes = certificate_payload_bytes(&payload)?;
        let signature = self.signing_key.sign(&payload_bytes);
        Ok(PlayerCertificate {
            payload,
            issuer_public_key: encode_base64(self.signing_key.verifying_key().as_bytes()),
            signature: encode_base64(&signature.to_bytes()),
        })
    }

    fn verify(&self, certificate: &PlayerCertificate) -> Result<(), McpError> {
        let expected_issuer = encode_base64(self.signing_key.verifying_key().as_bytes());
        if certificate.issuer_public_key != expected_issuer {
            return Err(McpError::invalid_params("certificate issuer is invalid"));
        }
        let payload_bytes = certificate_payload_bytes(&certificate.payload)?;
        let signature = decode_ed25519_signature(&certificate.signature, "certificate signature")?;
        self.signing_key
            .verifying_key()
            .verify(&payload_bytes, &signature)
            .map_err(|_| McpError::invalid_params("certificate signature is invalid"))
    }
}

fn mcp_tool_infos() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "swarm_get_snapshot".to_string(),
            description: "Get the visible world state for a player at the current tick".to_string(),
        },
        ToolInfo {
            name: "swarm_get_world_rules".to_string(),
            description: "Get the world rules and mods configuration".to_string(),
        },
        ToolInfo {
            name: "swarm_get_available_actions".to_string(),
            description: "List all WASM actions and MCP tools available to the player".to_string(),
        },
        ToolInfo {
            name: "swarm_explain_last_tick".to_string(),
            description: "Explain the last tick's results for a player".to_string(),
        },
        ToolInfo {
            name: "swarm_profile".to_string(),
            description: "Profile a player's current world state".to_string(),
        },
        ToolInfo {
            name: "swarm_dry_run_commands".to_string(),
            description: "Dry-run commands without mutating the world".to_string(),
        },
        ToolInfo {
            name: "swarm_get_docs".to_string(),
            description: "Get Swarm documentation and reference material".to_string(),
        },
        ToolInfo {
            name: "swarm_deploy".to_string(),
            description: "Deploy a WASM module for a player".to_string(),
        },
    ]
}

fn wasm_action_names() -> Vec<String> {
    [
        "Move", "Harvest", "Transfer", "Withdraw", "Attack", "Heal", "Spawn", "Build",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

pub fn swarm_get_available_actions(context: McpContext) -> AvailableActionsResult {
    AvailableActionsResult {
        tick: context.tick,
        player_id: context.player_id,
        wasm_actions: wasm_action_names(),
        mcp_tools: mcp_tool_infos(),
    }
}

pub fn swarm_explain_last_tick(world: &mut SwarmWorld, context: McpContext) -> TickExplanation {
    let snapshot = swarm_get_snapshot(world, context.clone());
    TickExplanation {
        tick: context.tick.saturating_sub(1),
        player_id: context.player_id,
        state_checksum: world.state_checksum(),
        visible_entity_count: snapshot.entities.len(),
        visible_tile_count: snapshot.visible_tiles.len(),
        accepted_commands: 0,
        rejected_commands: 0,
        notes: vec![
            "No persisted tick trace is attached to this in-process MCP server yet".to_string(),
            "Use swarm_dry_run_commands to validate candidate commands without mutating world"
                .to_string(),
        ],
    }
}

pub fn swarm_profile(
    world: &mut SwarmWorld,
    context: McpContext,
    deployed_modules: usize,
) -> ProfileResult {
    let snapshot = swarm_get_snapshot(world, context.clone());
    let owned_visible_drones = snapshot.entities.iter().filter(|entity| matches!(entity, VisibleEntity::Drone(drone) if drone.owner == context.player_id)).count();
    let owned_visible_structures = snapshot.entities.iter().filter(|entity| matches!(entity, VisibleEntity::Structure(structure) if structure.owner == Some(context.player_id))).count();
    ProfileResult {
        tick: context.tick,
        player_id: context.player_id,
        deployed_modules,
        pending_modules: deployed_modules,
        owned_visible_drones,
        owned_visible_structures,
        available_mcp_tools: mcp_tool_infos().len(),
        direct_gameplay_tools_enabled: false,
    }
}

pub fn swarm_dry_run_commands(
    world: &mut SwarmWorld,
    context: McpContext,
    params: DryRunCommandsParams,
) -> DryRunCommandsResult {
    let state_checksum_before = world.state_checksum();
    let mut results = Vec::new();
    for intent in params.commands {
        let raw = RawCommand {
            player_id: context.player_id,
            tick: context.tick,
            source: CommandSource::DryRun,
            auth: CommandAuth {
                source: CommandSource::DryRun,
                player_id: context.player_id,
                tick_submitted: context.tick,
                tick_target: context.tick,
            },
            sequence: intent.sequence,
            action: intent.action,
        };
        let sequence = raw.sequence;
        let cmd = raw.clone();
        match validate_command(world.app.world_mut(), cmd) {
            Ok(_) => results.push(DryRunCommandResult {
                sequence,
                command: raw,
                accepted: true,
                rejection: None,
            }),
            Err(rejection) => results.push(DryRunCommandResult {
                sequence,
                command: raw,
                accepted: false,
                rejection: Some(rejection),
            }),
        }
    }
    let state_checksum_after = world.state_checksum();
    DryRunCommandsResult {
        tick: context.tick,
        player_id: context.player_id,
        commands: results,
        state_checksum_before,
        state_checksum_after,
        mutated_world: state_checksum_before != state_checksum_after,
    }
}

pub fn swarm_get_docs(params: DocsParams) -> DocsResult {
    let topic = params.topic;
    let sections = match topic.as_str() {
        "mcp" => vec![
            DocsSection { title: "MCP contract".to_string(), body: "MCP exposes read/debug/deploy tools, but never direct gameplay actions such as move, attack, build, or spawn.".to_string() },
            DocsSection { title: "Eight phase-2 tools".to_string(), body: mcp_tool_infos().iter().map(|tool| format!("{}: {}", tool.name, tool.description)).collect::<Vec<_>>().join("\n") },
        ],
        "commands" => vec![DocsSection { title: "WASM CommandIntent actions".to_string(), body: wasm_action_names().join(", ") }],
        _ => vec![DocsSection { title: "Swarm docs".to_string(), body: "Topics: mcp, commands. See P0-3 MCP security contract and P0-8 Game API IDL.".to_string() }],
    };
    DocsResult { topic, sections }
}

fn default_docs_topic() -> String {
    "overview".to_string()
}

pub fn swarm_get_snapshot(world: &mut SwarmWorld, context: McpContext) -> VisibleWorldSnapshot {
    let snapshot = build_visible_snapshot(world, context.clone());
    let key = SnapshotKey::new(context.player_id, context.tick);
    let authoritative = world
        .app
        .world_mut()
        .resource_mut::<crate::hot_cache::InMemoryFoundationDb>()
        .write_visible_snapshot(snapshot);
    let mut cache = world
        .app
        .world_mut()
        .resource_mut::<crate::hot_cache::InMemoryDragonfly>();
    read_through_dragonfly(&mut *cache, key, authoritative)
}

fn build_visible_snapshot(world: &mut SwarmWorld, context: McpContext) -> VisibleWorldSnapshot {
    let room_id = RoomId(0);
    let visible_positions = visible_positions(world.app.world_mut(), context.player_id);
    let terrains = world.app.world().resource::<RoomTerrains>();
    let mut visible_tiles = terrains
        .0
        .iter()
        .flat_map(|(room_id, room)| {
            let visible_positions = &visible_positions;
            room.iter().filter_map(move |(x, y, terrain)| {
                visible_positions
                    .contains(&(*room_id, x, y))
                    .then_some(VisibleTile {
                        x,
                        y,
                        room_id: room_id.0,
                        terrain,
                    })
            })
        })
        .collect::<Vec<_>>();
    visible_tiles.sort();

    let visible_ids = visible_entity_ids(world.app.world_mut(), context.player_id, context.tick);
    let mut entities = visible_entities(world.app.world_mut(), &visible_positions, &visible_ids);
    entities.sort_by_key(entity_sort_key);

    VisibleWorldSnapshot {
        tick: context.tick,
        player_id: context.player_id,
        room_id: room_id.0,
        visibility_radius: VISIBILITY_RADIUS,
        visible_tiles,
        entities,
        local_storage: player_storage_snapshot(
            &world.app.world().resource::<PlayerLocalStorage>().0,
            context.player_id,
        ),
        global_storage: player_storage_snapshot(
            &world.app.world().resource::<PlayerGlobalStorage>().0,
            context.player_id,
        ),
        pending_global_transfers: world
            .app
            .world()
            .resource::<PendingGlobalTransfers>()
            .0
            .iter()
            .filter(|transfer| transfer.player_id == context.player_id)
            .map(|transfer| VisiblePendingGlobalTransfer {
                player_id: transfer.player_id,
                direction: format!("{:?}", transfer.direction),
                resource: transfer.resource.clone(),
                amount: transfer.amount,
                deliver_amount: transfer.deliver_amount,
                remaining_ticks: transfer.remaining_ticks,
            })
            .collect(),
    }
}

fn player_storage_snapshot(
    storage: &indexmap::IndexMap<PlayerId, indexmap::IndexMap<String, u32>>,
    player_id: PlayerId,
) -> BTreeMap<String, u32> {
    storage
        .get(&player_id)
        .map(|amounts| {
            amounts
                .iter()
                .map(|(name, amount)| (name.clone(), *amount))
                .collect()
        })
        .unwrap_or_default()
}

pub fn swarm_get_world_rules() -> WorldRules {
    let mut engine_config = BTreeMap::new();
    engine_config.insert("mcp_direct_gameplay_actions".to_string(), json!(false));
    engine_config.insert(
        "snapshot_visibility".to_string(),
        json!("player_visible_tiles_only"),
    );

    let mut base_config = BTreeMap::new();
    base_config.insert(
        "max_body_parts".to_string(),
        json!(crate::command::MAX_BODY_PARTS),
    );
    base_config.insert(
        "max_commands_per_player".to_string(),
        json!(crate::command::MAX_COMMANDS_PER_PLAYER),
    );
    base_config.insert(
        "max_drones_per_player".to_string(),
        json!(crate::command::MAX_DRONES_PER_PLAYER),
    );

    WorldRules {
        ruleset: "phase1".to_string(),
        room_size: DEFAULT_ROOM_SIZE,
        visibility_radius: VISIBILITY_RADIUS,
        max_wasm_bytes: MAX_WASM_BYTES,
        active_mods: vec![
            WorldRuleMod {
                id: "mcp_security_contract".to_string(),
                version: "phase1".to_string(),
                description: "MCP exposes deploy and safe read-only tools only".to_string(),
                config: engine_config,
            },
            WorldRuleMod {
                id: "base_world".to_string(),
                version: "phase1".to_string(),
                description: "Core room, command, and sandbox limits".to_string(),
                config: base_config,
            },
        ],
    }
}

pub fn is_visible_to(world: &mut World, player_id: PlayerId, position: Position) -> bool {
    is_position_visible_to(world, player_id, position)
}

pub fn visible_entities_for_player(world: &mut World, player_id: PlayerId) -> Vec<VisibleEntity> {
    let visible_positions = visible_positions(world, player_id);
    let visible_ids = visible_entity_ids(world, player_id, 0);
    let mut entities = visible_entities(world, &visible_positions, &visible_ids);
    entities.sort_by_key(entity_sort_key);
    entities
}

fn visible_entities(
    world: &mut World,
    visible_positions: &BTreeSet<(RoomId, i32, i32)>,
    visible_ids: &BTreeSet<Entity>,
) -> Vec<VisibleEntity> {
    let mut entities = Vec::new();

    for (entity, position, drone) in world.query::<(Entity, &Position, &Drone)>().iter(world) {
        if visible_ids.contains(&entity)
            || visible_positions.contains(&(position.room, position.x, position.y))
        {
            entities.push(VisibleEntity::Drone(VisibleDrone {
                id: object_id(entity),
                owner: drone.owner,
                position: visible_position(*position),
                body: drone.body.clone(),
                carry: drone.carry.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                carry_capacity: drone.carry_capacity,
                fatigue: drone.fatigue,
                hits: drone.hits,
                hits_max: drone.hits_max,
                spawning: drone.spawning,
            }));
        }
    }

    for (entity, position, structure) in
        world.query::<(Entity, &Position, &Structure)>().iter(world)
    {
        if visible_ids.contains(&entity)
            || visible_positions.contains(&(position.room, position.x, position.y))
        {
            entities.push(VisibleEntity::Structure(VisibleStructure {
                id: object_id(entity),
                structure_type: structure.structure_type,
                owner: structure.owner,
                position: visible_position(*position),
                hits: structure.hits,
                hits_max: structure.hits_max,
                energy: structure.energy,
                energy_capacity: structure.energy_capacity,
                cooldown: structure.cooldown,
            }));
        }
    }

    for (entity, position, source) in world.query::<(Entity, &Position, &Source)>().iter(world) {
        if visible_positions.contains(&(position.room, position.x, position.y)) {
            entities.push(VisibleEntity::Source(VisibleSource {
                id: object_id(entity),
                position: visible_position(*position),
                produces: source
                    .produces
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
                capacity: source.capacity,
                ticks_to_regeneration: source.ticks_to_regeneration,
            }));
        }
    }

    for (entity, position, resource) in world
        .query::<(Entity, &Position, &crate::components::Resource)>()
        .iter(world)
    {
        if visible_positions.contains(&(position.room, position.x, position.y)) {
            entities.push(VisibleEntity::Resource(VisibleResource {
                id: object_id(entity),
                position: visible_position(*position),
                amounts: resource
                    .amounts
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect(),
            }));
        }
    }

    for (entity, position, controller) in world
        .query::<(Entity, &Position, &Controller)>()
        .iter(world)
    {
        if visible_ids.contains(&entity)
            || visible_positions.contains(&(position.room, position.x, position.y))
        {
            entities.push(VisibleEntity::Controller(VisibleController {
                id: object_id(entity),
                owner: controller.owner,
                position: visible_position(*position),
                level: controller.level,
                progress: controller.progress,
                progress_total: controller.progress_total,
                safe_mode: controller.safe_mode,
            }));
        }
    }

    entities
}

fn visible_position(position: Position) -> VisiblePosition {
    VisiblePosition {
        x: position.x,
        y: position.y,
        room_id: position.room.0,
    }
}

fn entity_sort_key(entity: &VisibleEntity) -> (u8, ObjectId) {
    match entity {
        VisibleEntity::Drone(entity) => (0, entity.id),
        VisibleEntity::Structure(entity) => (1, entity.id),
        VisibleEntity::Source(entity) => (2, entity.id),
        VisibleEntity::Resource(entity) => (3, entity.id),
        VisibleEntity::Controller(entity) => (4, entity.id),
    }
}

fn error_response(id: Value, error: McpError) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(error),
    }
}

fn unix_timestamp_string() -> String {
    unix_timestamp_seconds().to_string()
}

fn unix_timestamp_seconds() -> u64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    seconds
}

fn opaque_web_token(kind: &str, provider: &str, subject: &str, seed: &str, now: u64) -> String {
    let material = format!("{kind}:{provider}:{subject}:{seed}:{now}");
    format!(
        "swarm_{kind}_{}",
        blake3::hash(material.as_bytes()).to_hex()
    )
}

fn oauth_player_id(provider: &str, subject: &str) -> PlayerId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(provider.as_bytes());
    hasher.update(b"\0");
    hasher.update(subject.as_bytes());
    let bytes = hasher.finalize();
    let mut id_bytes = [0_u8; 4];
    id_bytes.copy_from_slice(&bytes.as_bytes()[0..4]);
    u32::from_le_bytes(id_bytes)
}

fn certificate_payload_bytes(payload: &PlayerCertificatePayload) -> Result<Vec<u8>, McpError> {
    serde_json::to_vec(payload).map_err(|error| McpError::invalid_params(error.to_string()))
}

fn verify_wasm_signature(
    certificate: &PlayerCertificate,
    wasm_hash: &[u8],
    wasm_signature: &str,
) -> Result<(), McpError> {
    let verifying_key = decode_ed25519_public_key(
        &certificate.payload.client_public_key,
        "certificate client_public_key",
    )?;
    let signature = decode_ed25519_signature(wasm_signature, "wasm_signature")?;
    verifying_key
        .verify(wasm_hash, &signature)
        .map_err(|_| McpError::invalid_params("wasm_signature is invalid"))
}

fn decode_ed25519_public_key(input: &str, field: &str) -> Result<VerifyingKey, McpError> {
    let bytes = decode_base64_with_message(input, field)?;
    let key_bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| McpError::invalid_params(format!("{field} must be 32 bytes")))?;
    VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| McpError::invalid_params(format!("{field} is invalid")))
}

fn decode_ed25519_signature(input: &str, field: &str) -> Result<Signature, McpError> {
    let bytes = decode_base64_with_message(input, field)?;
    let signature_bytes: [u8; 64] = bytes
        .try_into()
        .map_err(|_| McpError::invalid_params(format!("{field} must be 64 bytes")))?;
    Ok(Signature::from_bytes(&signature_bytes))
}

fn decode_base64_with_message(input: &str, field: &str) -> Result<Vec<u8>, McpError> {
    decode_base64(input)
        .map_err(|_| McpError::invalid_params(format!("{field} is not valid base64")))
}

fn encode_base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);

        output.push(ALPHABET[(a >> 2) as usize] as char);
        output.push(ALPHABET[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(c & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

fn decode_base64(input: &str) -> Result<Vec<u8>, McpError> {
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err(McpError::invalid_params("wasm_bytes is not valid base64"));
    }

    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut chunks = bytes.chunks_exact(4).peekable();
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none();
        let a = base64_value(chunk[0])?;
        let b = base64_value(chunk[1])?;
        let c = if chunk[2] == b'=' {
            64
        } else {
            base64_value(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            64
        } else {
            base64_value(chunk[3])?
        };
        if (chunk[2] == b'=' && chunk[3] != b'=')
            || (!last && (chunk[2] == b'=' || chunk[3] == b'='))
        {
            return Err(McpError::invalid_params("wasm_bytes is not valid base64"));
        }

        output.push((a << 2) | (b >> 4));
        if c != 64 {
            output.push(((b & 0x0f) << 4) | (c >> 2));
        }
        if d != 64 {
            output.push(((c & 0x03) << 6) | d);
        }
    }

    Ok(output)
}

fn base64_value(byte: u8) -> Result<u8, McpError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(McpError::invalid_params("wasm_bytes is not valid base64")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{create_world, Structure, StructureType};

    fn spawn_structure(world: &mut SwarmWorld, owner: Option<PlayerId>, x: i32, y: i32) {
        world.app.world_mut().spawn((
            Position {
                x,
                y,
                room: RoomId(0),
            },
            Structure {
                structure_type: StructureType::Spawn,
                owner,
                hits: 5_000,
                hits_max: 5_000,
                energy: Some(300),
                energy_capacity: Some(300),
                cooldown: 0,
            },
        ));
    }

    fn test_signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn login_with_key(server: &mut McpServer, client_key: &SigningKey) -> OAuth2LoginResult {
        server
            .swarm_oauth2_login(OAuth2LoginParams {
                provider: "test-oauth".to_string(),
                subject: "player@example.com".to_string(),
                access_token: "opaque-test-token".to_string(),
                client_public_key: encode_base64(client_key.verifying_key().as_bytes()),
            })
            .expect("login should issue certificate")
    }

    fn signed_deploy_params(
        certificate: PlayerCertificate,
        client_key: &SigningKey,
    ) -> DeployParams {
        let wasm_bytes = b"\0asm\x01\0\0\0";
        let wasm_hash = blake3::hash(wasm_bytes);
        let wasm_signature = client_key.sign(wasm_hash.as_bytes());
        DeployParams {
            wasm_bytes: encode_base64(wasm_bytes),
            certificate,
            wasm_signature: encode_base64(&wasm_signature.to_bytes()),
            language: "rust".to_string(),
            version_tag: "v1".to_string(),
            room_id: 0,
        }
    }

    #[test]
    fn snapshot_filters_entities_and_terrain_by_player_visibility() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        world.spawn_drone(2, 12, 10, vec![BodyPart::Move]);
        world.spawn_drone(2, 40, 40, vec![BodyPart::Move]);

        let snapshot = swarm_get_snapshot(
            &mut world,
            McpContext {
                player_id: 1,
                tick: 7,
            },
        );

        assert_eq!(snapshot.tick, 7);
        assert!(snapshot
            .visible_tiles
            .iter()
            .any(|tile| tile.x == 10 && tile.y == 10));
        assert!(!snapshot
            .visible_tiles
            .iter()
            .any(|tile| tile.x == 40 && tile.y == 40));
        assert!(snapshot.visible_tiles.len() < (DEFAULT_ROOM_SIZE * DEFAULT_ROOM_SIZE) as usize);

        let drone_positions = snapshot
            .entities
            .iter()
            .filter_map(|entity| match entity {
                VisibleEntity::Drone(drone) => {
                    Some((drone.owner, drone.position.x, drone.position.y))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(drone_positions.contains(&(1, 10, 10)));
        assert!(drone_positions.contains(&(2, 12, 10)));
        assert!(!drone_positions.contains(&(2, 40, 40)));
    }

    #[test]
    fn swarm_get_snapshot_uses_dragonfly_cache_after_fdb_backfill() {
        let mut world = create_world();
        world.spawn_drone(1, 10, 10, vec![BodyPart::Move]);
        let context = McpContext {
            player_id: 1,
            tick: 11,
        };

        let first = swarm_get_snapshot(&mut world, context.clone());
        let stats = world
            .app
            .world()
            .resource::<crate::hot_cache::InMemoryDragonfly>()
            .stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.refreshes, 1);

        let second = swarm_get_snapshot(&mut world, context);
        let stats = world
            .app
            .world()
            .resource::<crate::hot_cache::InMemoryDragonfly>()
            .stats();
        assert_eq!(second, first);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.refreshes, 1);
    }

    #[test]
    fn owned_structure_extends_visibility_without_leaking_full_map() {
        let mut world = create_world();
        spawn_structure(&mut world, Some(1), 35, 35);

        let snapshot = swarm_get_snapshot(
            &mut world,
            McpContext {
                player_id: 1,
                tick: 1,
            },
        );

        assert!(is_visible_to(
            world.app.world_mut(),
            1,
            Position {
                x: 35,
                y: 35,
                room: RoomId(0)
            }
        ));
        assert!(snapshot
            .visible_tiles
            .iter()
            .any(|tile| tile.x == 35 && tile.y == 35));
        assert!(!snapshot
            .visible_tiles
            .iter()
            .any(|tile| tile.x == 0 && tile.y == 0));
    }

    #[test]
    fn deploy_validates_and_stores_wasm_for_next_tick_loading() {
        let world = create_world();
        let issuer = test_signing_key(1);
        let client_key = test_signing_key(2);
        let mut server = McpServer::with_issuer_for_tests(issuer, 1_000);
        let login = login_with_key(&mut server, &client_key);
        let result = server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 11,
                },
                signed_deploy_params(login.certificate.clone(), &client_key),
            )
            .expect("deploy should succeed");

        assert_eq!(result.status, "pending_next_tick");
        assert_eq!(server.modules().len(), 1);
        assert_eq!(server.modules()[0].module_id, result.module_id);
        assert_eq!(server.modules()[0].load_after_tick, 12);
        assert_eq!(server.modules()[0].wasm_bytes, b"\0asm\x01\0\0\0");
        assert_eq!(server.modules()[0].certificate, login.certificate);
        assert!(!server.modules()[0].wasm_signature.is_empty());
        assert_eq!(
            server.modules()[0].wasm_hash,
            blake3::hash(b"\0asm\x01\0\0\0").to_hex().to_string()
        );
    }

    #[test]
    fn deploy_rejects_invalid_base64_and_non_wasm() {
        let world = create_world();
        let issuer = test_signing_key(3);
        let client_key = test_signing_key(4);
        let mut server = McpServer::with_issuer_for_tests(issuer, 1_000);
        let login = login_with_key(&mut server, &client_key);
        let context = McpContext {
            player_id: login.player_id,
            tick: 0,
        };
        let valid_params = signed_deploy_params(login.certificate, &client_key);

        assert!(server
            .swarm_deploy(
                &world,
                context.clone(),
                DeployParams {
                    wasm_bytes: "not base64".to_string(),
                    ..valid_params.clone()
                },
            )
            .is_err());
        assert!(server
            .swarm_deploy(
                &world,
                context,
                DeployParams {
                    wasm_bytes: "YWJj".to_string(),
                    ..valid_params
                },
            )
            .is_err());
    }

    #[test]
    fn oauth2_login_issues_ed25519_certificate_with_24h_expiry() {
        let issuer = test_signing_key(5);
        let client_key = test_signing_key(6);
        let mut server = McpServer::with_issuer_for_tests(issuer, 10_000);

        let login = login_with_key(&mut server, &client_key);

        assert_eq!(login.certificate.payload.issued_at, 10_000);
        assert_eq!(
            login.certificate.payload.expires_at,
            10_000 + CERTIFICATE_TTL_SECONDS
        );
        assert_eq!(login.certificate.payload.player_id, login.player_id);
        assert_eq!(login.certificate.payload.audience, CERTIFICATE_AUDIENCE);
        server
            .verify_certificate_for_player(&login.certificate, login.player_id)
            .expect("issued certificate should verify");
    }

    #[test]
    fn deploy_rejects_invalid_wasm_signature() {
        let world = create_world();
        let issuer = test_signing_key(7);
        let client_key = test_signing_key(8);
        let wrong_key = test_signing_key(9);
        let mut server = McpServer::with_issuer_for_tests(issuer, 20_000);
        let login = login_with_key(&mut server, &client_key);
        let mut params = signed_deploy_params(login.certificate, &wrong_key);

        let error = server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 1,
                },
                params.clone(),
            )
            .expect_err("wrong key must not verify against certificate public key");
        assert_eq!(error.message, "wasm_signature is invalid");

        params.wasm_signature = "not base64".to_string();
        assert!(server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 1,
                },
                params,
            )
            .is_err());
    }

    #[test]
    fn deploy_rejects_expired_certificate() {
        let world = create_world();
        let issuer = test_signing_key(10);
        let client_key = test_signing_key(11);
        let mut issuing_server = McpServer::with_issuer_for_tests(issuer.clone(), 30_000);
        let login = login_with_key(&mut issuing_server, &client_key);
        let mut verifying_server =
            McpServer::with_issuer_for_tests(issuer, 30_000 + CERTIFICATE_TTL_SECONDS);

        let error = verifying_server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 1,
                },
                signed_deploy_params(login.certificate, &client_key),
            )
            .expect_err("expired certificate should be rejected");

        assert_eq!(error.message, "certificate is expired");
    }

    #[test]
    fn world_rules_expose_safe_readable_configuration() {
        let rules = swarm_get_world_rules();

        assert_eq!(rules.ruleset, "phase1");
        assert_eq!(rules.room_size, DEFAULT_ROOM_SIZE);
        assert!(rules.active_mods.iter().any(|module| {
            module.id == "mcp_security_contract"
                && module.config.get("mcp_direct_gameplay_actions") == Some(&json!(false))
        }));
    }

    #[test]
    fn json_rpc_dispatches_only_phase1_mcp_tools() {
        let mut world = create_world();
        let mut server = McpServer::new();
        let context = McpContext {
            player_id: 1,
            tick: 0,
        };

        let ok = server.handle_json_rpc(
            &mut world,
            context.clone(),
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!(1),
                method: "swarm_get_world_rules".to_string(),
                params: Value::Null,
            },
        );
        assert!(ok.result.is_some());
        assert!(ok.error.is_none());

        let denied = server.handle_json_rpc(
            &mut world,
            context,
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: json!(2),
                method: "swarm_move".to_string(),
                params: Value::Null,
            },
        );
        assert!(denied.result.is_none());
        assert_eq!(denied.error.unwrap().code, -32601);
    }

    #[test]
    fn oauth2_callback_issues_web_session_and_certificate() {
        let issuer = test_signing_key(13);
        let client_key = test_signing_key(14);
        let mut server = McpServer::with_issuer_for_tests(issuer, 30_000);
        let login = server
            .swarm_oauth2_callback(OAuth2CallbackParams {
                provider: "github".to_string(),
                code: "github-code".to_string(),
                state: "csrf".to_string(),
                redirect_uri: "https://swarm.example/auth/callback".to_string(),
                client_public_key: encode_base64(client_key.verifying_key().as_bytes()),
            })
            .expect("callback issues tokens and cert");
        assert_eq!(login.session.audience, WEB_TOKEN_AUDIENCE);
        assert_eq!(login.session.provider, "github");
        assert_eq!(
            login.session.access_token_expires_at,
            30_000 + WEB_ACCESS_TOKEN_TTL_SECONDS
        );
        assert_eq!(
            login.session.refresh_token_expires_at,
            30_000 + WEB_REFRESH_TOKEN_TTL_SECONDS
        );
        assert_eq!(
            login.certificate.payload.expires_at,
            30_000 + CERTIFICATE_TTL_SECONDS
        );
        server
            .verify_certificate_for_player(&login.certificate, login.player_id)
            .unwrap();
    }

    #[test]
    fn token_refresh_renews_access_token_and_24h_certificate() {
        let issuer = test_signing_key(15);
        let client_key = test_signing_key(16);
        let mut server = McpServer::with_issuer_for_tests(issuer, 40_000);
        let login = login_with_key(&mut server, &client_key);
        server.now_seconds = Some(41_000);
        let refreshed = server
            .swarm_token_refresh(TokenRefreshParams {
                refresh_token: login.session.refresh_token.clone(),
                client_public_key: encode_base64(client_key.verifying_key().as_bytes()),
            })
            .expect("refresh renews cert");
        assert_ne!(refreshed.session.access_token, login.session.access_token);
        assert_eq!(refreshed.certificate.payload.issued_at, 41_000);
        assert_eq!(
            refreshed.certificate.payload.expires_at,
            41_000 + CERTIFICATE_TTL_SECONDS
        );
        assert_eq!(refreshed.renew_after_seconds, CERTIFICATE_TTL_SECONDS);
    }

    #[test]
    fn revoke_blocks_refresh_and_certificate_deploy() {
        let world = create_world();
        let issuer = test_signing_key(17);
        let client_key = test_signing_key(18);
        let mut server = McpServer::with_issuer_for_tests(issuer, 50_000);
        let login = login_with_key(&mut server, &client_key);
        let revoked = server
            .swarm_auth_revoke(RevokeAuthParams {
                refresh_token: Some(login.session.refresh_token.clone()),
                certificate: Some(login.certificate.clone()),
            })
            .unwrap();
        assert!(revoked.revoked_session && revoked.revoked_certificate);
        assert!(server
            .swarm_token_refresh(TokenRefreshParams {
                refresh_token: login.session.refresh_token,
                client_public_key: encode_base64(client_key.verifying_key().as_bytes())
            })
            .is_err());
        let error = server
            .swarm_deploy(
                &world,
                McpContext {
                    player_id: login.player_id,
                    tick: 1,
                },
                signed_deploy_params(login.certificate, &client_key),
            )
            .expect_err("revoked cert rejected");
        assert_eq!(error.message, "certificate is revoked");
    }
}
