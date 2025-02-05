use axum::{Router, routing::get};
use fabricia_backend_service::BackendServices;

pub mod auth;
mod branch;
pub mod error;

pub fn api_router() -> Router<BackendServices> {
	Router::new()
		.route("/", get(handler))
		.route("/branch", get(branch::list_branches))
		.route(
			"/branch/{branch}",
			get(branch::get_branch)
				.put(branch::new_branch)
				.patch(branch::update_branch_config)
				.delete(branch::delete_branch),
		)
}

async fn handler() -> &'static str {
	concat!("Fabricia Crayon ", env!("CARGO_PKG_VERSION"))
}
