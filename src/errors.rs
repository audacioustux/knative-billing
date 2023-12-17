use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use uuid::Uuid;

pub enum ErrorKind {
    InternalServerError(anyhow::Error),
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error_id: Uuid,
    pub message: String,
}

pub struct AppError(ErrorKind);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let Self(err) = self;

        match err {
            ErrorKind::InternalServerError(err) => {
                let error_id = Uuid::new_v4();
                tracing::error!("{}: Internal Server Error: {}", error_id, err);

                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error_id,
                        message: "Internal Server Error".into(),
                    }),
                )
            }
        }
        .into_response()
    }
}

impl<E> From<E> for ErrorKind
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        ErrorKind::InternalServerError(err.into())
    }
}

impl<E> From<E> for AppError
where
    E: Into<ErrorKind>,
{
    fn from(err: E) -> Self {
        AppError(err.into())
    }
}
