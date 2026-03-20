use crate::foundation::*;
use crate::patch::{PhaseContext, StateCommand};
use std::sync::Arc;

pub trait TypedScheduledActionHandler<A>: Send + Sync + 'static
where
    A: ScheduledActionSpec,
{
    fn handle_typed(
        &self,
        ctx: &PhaseContext,
        payload: A::Payload,
    ) -> Result<StateCommand, StateError>;
}

pub trait TypedEffectHandler<E>: Send + Sync + 'static
where
    E: EffectSpec,
{
    fn handle_typed(&self, payload: E::Payload, snapshot: &Snapshot) -> Result<(), String>;
}

pub(crate) trait ErasedTypedScheduledActionHandler: Send + Sync + 'static {
    fn handle_erased(
        &self,
        ctx: &PhaseContext,
        payload: JsonValue,
    ) -> Result<StateCommand, StateError>;
}

pub(crate) struct TypedScheduledActionAdapter<A, H> {
    pub(crate) handler: H,
    pub(crate) _marker: std::marker::PhantomData<A>,
}

impl<A, H> ErasedTypedScheduledActionHandler for TypedScheduledActionAdapter<A, H>
where
    A: ScheduledActionSpec,
    H: TypedScheduledActionHandler<A>,
{
    fn handle_erased(
        &self,
        ctx: &PhaseContext,
        payload: JsonValue,
    ) -> Result<StateCommand, StateError> {
        self.handler.handle_typed(ctx, A::decode_payload(payload)?)
    }
}

pub(crate) trait ErasedTypedEffectHandler: Send + Sync + 'static {
    fn handle_erased(&self, payload: JsonValue, snapshot: &Snapshot) -> Result<(), String>;
}

pub(crate) struct TypedEffectAdapter<E, H> {
    pub(crate) handler: H,
    pub(crate) _marker: std::marker::PhantomData<E>,
}

impl<E, H> ErasedTypedEffectHandler for TypedEffectAdapter<E, H>
where
    E: EffectSpec,
    H: TypedEffectHandler<E>,
{
    fn handle_erased(&self, payload: JsonValue, snapshot: &Snapshot) -> Result<(), String> {
        let payload = decode_json::<E::Payload>(E::KEY, payload).map_err(|err| err.to_string())?;
        self.handler.handle_typed(payload, snapshot)
    }
}

pub(crate) type ScheduledActionHandlerArc = Arc<dyn ErasedTypedScheduledActionHandler>;
pub(crate) type EffectHandlerArc = Arc<dyn ErasedTypedEffectHandler>;
