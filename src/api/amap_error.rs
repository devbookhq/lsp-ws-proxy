//! AMapErr exists to allow ? operations in main code.

use std::convert::From;
use std::sync::mpsc::{RecvError, SendError};

#[derive(Debug)]
pub enum AMapErr {
    SendErr,
    RecvErr,
    NotFound,
}

impl<T> From<SendError<T>> for AMapErr {
    fn from(_: SendError<T>) -> Self {
        AMapErr::SendErr
    }
}

impl From<RecvError> for AMapErr {
    fn from(_: RecvError) -> Self {
        AMapErr::RecvErr
    }
}
