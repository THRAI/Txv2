#![no_std]

pub mod ast {
    pub struct AstSlot;
}

pub mod preempt {
    pub struct PreemptionPoint;
}

pub mod task {
    pub struct TaskId(pub usize);
}

pub mod wait {
    pub struct WaitToken;
}
