/// Stable process codes shared by the parser, runtime adapter, and tests.
pub const SUCCESS: i32 = 0;
pub const ERROR: i32 = 1;
pub const GOAL_BLOCKED: i32 = 3;
pub const GOAL_PAUSED: i32 = 6;
pub const SIGHUP: i32 = 129;
pub const SIGINT: i32 = 130;
pub const SIGQUIT: i32 = 131;
pub const SIGTERM: i32 = 143;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    Complete,
    Blocked,
    Paused,
}

impl GoalStatus {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Complete => SUCCESS,
            Self::Blocked => GOAL_BLOCKED,
            Self::Paused => GOAL_PAUSED,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationSignal {
    Hangup,
    Interrupt,
    Quit,
    Terminate,
}

impl TerminationSignal {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Hangup => SIGHUP,
            Self::Interrupt => SIGINT,
            Self::Quit => SIGQUIT,
            Self::Terminate => SIGTERM,
        }
    }
}
