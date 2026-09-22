use thiserror::Error;

#[derive(Error, Debug)]
#[allow(dead_code)]
pub enum GameError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Authentication error: {0}")]
    Auth(String),

    // ---- account/nickname outcomes (typed so the HTTP layer maps an HONEST status, not a
    // string match that a reworded message would silently break). NicknameTaken→409,
    // NicknameInvalid→400, InvalidSession→401, and a leaked Database error→500 (NOT 401, which
    // would wrongly tell the client its session expired during a transient DB outage). ----
    #[error("Nickname already taken")]
    NicknameTaken,

    /// Length/charset rejection. Carries the stable, client-safe reason for the 400 body.
    #[error("{0}")]
    NicknameInvalid(&'static str),

    #[error("Invalid session")]
    InvalidSession,

    #[error("Room is full")]
    RoomFull,

    #[error("Room not found")]
    RoomNotFound,

    #[error("Player not found")]
    PlayerNotFound,

    #[error("Invalid action: {0}")]
    InvalidAction(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("QUIC error: {0}")]
    Quic(String),
}

pub type GameResult<T> = Result<T, GameError>;
