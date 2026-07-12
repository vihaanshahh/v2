#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use commands::serve::ProxyState;

fn main() {
    tauri::Builder::default()
        .manage(ProxyState::default())
        .invoke_handler(tauri::generate_handler![
            commands::scan::scan,
            commands::models::models_installed,
            commands::models::models_loaded,
            commands::models::model_fit_check,
            commands::models::model_pull,
            commands::models::model_rm,
            commands::models::model_stop,
            commands::models::model_chat,
            commands::models::chat_targets,
            commands::models::chat_send,
            commands::serve::serve_start,
            commands::serve::serve_stop,
            commands::serve::serve_status,
            commands::serve::usage_summary,
            commands::serve::doctor,
            commands::serve::endpoint_banner,
            commands::mesh::mesh_status,
            commands::mesh::mesh_peers,
            commands::mesh::mesh_id,
            commands::mesh::mesh_init,
            commands::mesh::mesh_invite,
            commands::mesh::mesh_join,
            commands::mesh::mesh_peer_add,
            commands::mesh::mesh_revoke,
            commands::mesh::mesh_pause,
            commands::mesh::mesh_resume,
            commands::mesh::mesh_federation_list,
            commands::mesh::mesh_federation_add,
        ])
        .run(tauri::generate_context!())
        .expect("error while running v2 desktop");
}
