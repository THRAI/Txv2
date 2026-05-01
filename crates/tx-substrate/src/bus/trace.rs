use core::marker::PhantomData;

/// Marker for structured payloads accepted by a typed tracepoint.
///
/// Trace payloads are copied into the trace publication call. The current
/// implementation is a no-op carrier; future subscriber/nop-patching support
/// can consume the same typed payload shape without changing call sites.
pub trait TracePayload: Copy {}

impl TracePayload for () {}

/// Static declaration for one typed tracepoint payload.
#[derive(Debug, Eq, PartialEq)]
pub struct TraceDeclaration<P> {
    name: &'static str,
    _payload: PhantomData<fn(P)>,
}

impl<P> Clone for TraceDeclaration<P> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<P> Copy for TraceDeclaration<P> {}

impl<P: TracePayload> TraceDeclaration<P> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _payload: PhantomData,
        }
    }

    pub const fn name(self) -> &'static str {
        self.name
    }
}

/// Passive typed trace publication carrier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawTrace<P = ()> {
    declaration: TraceDeclaration<P>,
}

impl<P: TracePayload> RawTrace<P> {
    pub const fn new(declaration: TraceDeclaration<P>) -> Self {
        Self { declaration }
    }

    pub const fn declaration(self) -> TraceDeclaration<P> {
        self.declaration
    }

    pub fn emit(&self, payload: P) {
        let _ = payload;
    }
}
