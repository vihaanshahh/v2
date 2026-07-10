//! Pure data behind `v2 doctor` — one status line per subsystem (Ollama,
//! identity, mesh membership, policy, abuse limits). No I/O beyond the reads
//! each subsystem already does; no printing. `main.rs`'s `doctor()` prints
//! this, and the desktop app's `doctor` command returns it as JSON.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warn,
    Bad,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorLine {
    pub status: Status,
    pub label: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub ollama: DoctorLine,
    pub identity: DoctorLine,
    pub mesh: DoctorLine,
    pub policy: DoctorLine,
    pub abuse: DoctorLine,
}

fn line(status: Status, label: &str, message: String) -> DoctorLine {
    DoctorLine { status, label: label.to_string(), message }
}

pub fn doctor_report(host: &str) -> DoctorReport {
    use crate::mesh;

    let ollama = match crate::ollama::fetch_local(host) {
        Ok(models) => line(Status::Ok, "ollama", format!("reachable at {host} · {} models", models.len())),
        Err(e) => line(Status::Bad, "ollama", format!("{e} — start it with `ollama serve`")),
    };

    let identity = match mesh::identity::NodeKey::load_or_create() {
        Ok(node) => line(Status::Ok, "identity", format!("node {}", mesh::short_id(&node.public_b64()))),
        Err(e) => line(Status::Bad, "identity", e),
    };

    let mesh_line = match mesh::identity::MeshIdentity::load() {
        Ok(Some(ident)) => {
            let now = crate::usage::now_unix();
            match ident.org_pub_bytes().and_then(|org| ident.cert.verify(&org, now)) {
                Ok(()) => {
                    let h = ident.cert.expiry.saturating_sub(now) / 3600;
                    line(Status::Ok, "mesh", format!("member of org {} · cert valid {h}h", mesh::short_id(&ident.org_pub)))
                }
                Err(e) => line(Status::Warn, "mesh", format!("membership cert problem: {e}")),
            }
        }
        Ok(None) => line(Status::Warn, "mesh", "not a member (run `v2 mesh init` or `v2 mesh join`)".into()),
        Err(e) => line(Status::Warn, "mesh", e),
    };

    let (policy, abuse) = match crate::policy::Policy::load() {
        Ok(p) => {
            let policy = line(
                Status::Ok,
                "policy",
                format!(
                    "{} remote job · {:.0}% VRAM cap · yield_to_local={}",
                    p.serve.max_concurrent_remote,
                    p.serve.max_vram_fraction * 100.0,
                    p.availability.yield_to_local
                ),
            );
            let a = &p.abuse;
            let lists = if !a.deny_nodes.is_empty() || !a.only_nodes.is_empty() {
                format!(" · {} deny / {} allow", a.deny_nodes.len(), a.only_nodes.len())
            } else {
                String::new()
            };
            let abuse = line(
                Status::Ok,
                "abuse",
                format!(
                    "{}/min per IP · {} conns ({}/IP) · ban after {} strikes{}",
                    a.handshake_rate_per_min,
                    a.max_connections,
                    a.max_connections_per_ip,
                    a.strike_limit,
                    lists,
                ),
            );
            (policy, abuse)
        }
        Err(e) => (
            line(Status::Bad, "policy", format!("{e} (serving will refuse to start)")),
            line(Status::Bad, "abuse", "unavailable — policy failed to load".into()),
        ),
    };

    DoctorReport { ollama, identity, mesh: mesh_line, policy, abuse }
}
