//! Application error type. Errors render as a themed error page (or JSON for `/api`).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    NoPermission(String),
    /// A user-facing validation/business error; shown in an error box.
    #[error("{0}")]
    User(String),
    #[error("Your request could not be verified. Please go back, refresh the page and try again.")]
    Csrf,
    #[error("Too many requests. Please slow down.")]
    RateLimited,
    #[error("Please log in to continue.")]
    LoginRequired,
    #[error(transparent)]
    Db(sqlx::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn user(msg: impl Into<String>) -> Self {
        AppError::User(msg.into())
    }
    pub fn not_found(what: &str) -> Self {
        AppError::NotFound(format!("The specified {what} does not exist."))
    }
    pub fn no_perm() -> Self {
        AppError::NoPermission("You do not have permission to access this page.".into())
    }
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::NoPermission(_) => StatusCode::FORBIDDEN,
            AppError::LoginRequired => StatusCode::UNAUTHORIZED,
            AppError::User(_) => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::Csrf => StatusCode::FORBIDDEN,
            AppError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            AppError::Db(_) | AppError::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    /// Message safe to show to end users (internal errors are not leaked).
    pub fn public_message(&self) -> String {
        match self {
            AppError::Db(_) | AppError::Other(_) => {
                "An internal error occurred. The administrator has been notified.".into()
            }
            e => e.to_string(),
        }
    }
}

/// SQLSTATE raised by the System account guards (see migrations/0005_system_user.sql).
pub const SYSTEM_PROTECTED: &str = "RBSYS";

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        if let Some(db) = e.as_database_error()
            && db.code().as_deref() == Some(SYSTEM_PROTECTED)
        {
            return AppError::User(db.message().to_string());
        }
        AppError::Db(e)
    }
}

impl From<minijinja::Error> for AppError {
    fn from(e: minijinja::Error) -> Self {
        AppError::Other(anyhow::anyhow!("template error: {e:#}"))
    }
}

/// Fallback conversion used only when the error escapes the page renderer
/// (the error-page middleware normally renders a themed page instead).
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if matches!(self, AppError::Db(_) | AppError::Other(_)) {
            tracing::error!(error = ?self, "request failed");
        }
        let status = self.status();
        let mut resp = (status, self.public_message()).into_response();
        resp.extensions_mut().insert(ErrorMarker(
            self.public_message(),
            status,
            matches!(self, AppError::LoginRequired),
        ));
        resp
    }
}

/// Carried in response extensions so the layout middleware can render a themed error page.
#[derive(Clone, Debug)]
pub struct ErrorMarker(pub String, pub StatusCode, pub bool);
