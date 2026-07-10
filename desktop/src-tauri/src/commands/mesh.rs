use v2::mesh::client::{self, MeshStatus, PeerCard};
use v2::mesh::identity::{FederatedOrg, FederationList, NodeKey};

#[tauri::command]
pub fn mesh_status() -> Result<MeshStatus, String> {
    client::status_data()
}

#[tauri::command]
pub fn mesh_peers() -> Result<Vec<PeerCard>, String> {
    client::peers_data()
}

#[tauri::command]
pub fn mesh_id() -> Result<String, String> {
    Ok(NodeKey::load_or_create()?.public_b64())
}

#[tauri::command]
pub fn mesh_init() -> Result<(), String> {
    client::init()
}

#[tauri::command]
pub fn mesh_invite(addr: Option<String>, via_relay: Option<String>, ttl_secs: Option<u64>) -> Result<String, String> {
    client::invite_ticket(addr.as_deref(), via_relay.as_deref(), ttl_secs.unwrap_or(86_400))
}

#[tauri::command]
pub fn mesh_join(ticket: String) -> Result<(), String> {
    client::join(&ticket)
}

#[tauri::command]
pub fn mesh_peer_add(addr: String) -> Result<(), String> {
    client::peer_add(&addr)
}

#[tauri::command]
pub fn mesh_revoke(node: String) -> Result<(), String> {
    client::revoke(&node)
}

#[tauri::command]
pub fn mesh_pause() -> Result<(), String> {
    client::pause()
}

#[tauri::command]
pub fn mesh_resume() -> Result<(), String> {
    client::resume()
}

#[tauri::command]
pub fn mesh_federation_list() -> Vec<FederatedOrg> {
    FederationList::load().orgs
}

#[tauri::command]
pub fn mesh_federation_add(org: String, note: Option<String>, models: Vec<String>) -> Result<(), String> {
    client::federation_add(&org, note.as_deref().unwrap_or(""), &models)
}
