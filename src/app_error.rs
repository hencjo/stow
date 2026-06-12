use std::io;

#[derive(Debug)]
pub enum AppError {
    Io(io::Error),
    Msg(String),
}

impl AppError {
    pub fn msg<T: Into<String>>(msg: T) -> Self {
        AppError::Msg(msg.into())
    }
}

impl From<io::Error> for AppError {
    fn from(value: io::Error) -> Self {
        AppError::Io(value)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Io(err) => write!(f, "{err}"),
            AppError::Msg(msg) => write!(f, "{msg}"),
        }
    }
}
