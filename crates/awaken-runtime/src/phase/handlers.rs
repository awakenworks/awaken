use std::sync::Arc;

use async_trait::async_trait;

use crate::state::{Snapshot, StateCommand};
use awaken_contract::StateError;
use awaken_contract::model::{EffectSpec, JsonValue, ScheduledActionSpec, decode_json};

use super::PhaseContext;

#[async_trait]
pub trait TypedScheduledActionHandler<A>: Send + Sync + 'static
where
    A: ScheduledActionSpec,
{
    async fn handle_typed(
        &self,
        ctx: &PhaseContext,
        payload: A::Payload,
    ) -> Result<StateCommand, StateError>;
}

#[async_trait]
pub trait TypedEffectHandler<E>: Send + Sync + 'static
where
    E: EffectSpec,
{
    async fn handle_typed(&self, payload: E::Payload, snapshot: &Snapshot) -> Result<(), String>;
}

#[async_trait]
pub(crate) trait ErasedTypedScheduledActionHandler: Send + Sync + 'static {
    async fn handle_erased(
        &self,
        ctx: &PhaseContext,
        payload: JsonValue,
    ) -> Result<StateCommand, StateError>;
}

pub(crate) struct TypedScheduledActionAdapter<A, H> {
    pub(crate) handler: H,
    pub(crate) _marker: std::marker::PhantomData<A>,
}

#[async_trait]
impl<A, H> ErasedTypedScheduledActionHandler for TypedScheduledActionAdapter<A, H>
where
    A: ScheduledActionSpec,
    H: TypedScheduledActionHandler<A>,
{
    async fn handle_erased(
        &self,
        ctx: &PhaseContext,
        payload: JsonValue,
    ) -> Result<StateCommand, StateError> {
        self.handler
            .handle_typed(ctx, A::decode_payload(payload)?)
            .await
    }
}

#[async_trait]
pub(crate) trait ErasedTypedEffectHandler: Send + Sync + 'static {
    async fn handle_erased(&self, payload: JsonValue, snapshot: &Snapshot) -> Result<(), String>;
}

pub(crate) struct TypedEffectAdapter<E, H> {
    pub(crate) handler: H,
    pub(crate) _marker: std::marker::PhantomData<E>,
}

#[async_trait]
impl<E, H> ErasedTypedEffectHandler for TypedEffectAdapter<E, H>
where
    E: EffectSpec,
    H: TypedEffectHandler<E>,
{
    async fn handle_erased(&self, payload: JsonValue, snapshot: &Snapshot) -> Result<(), String> {
        let payload = decode_json::<E::Payload>(E::KEY, payload).map_err(|err| err.to_string())?;
        self.handler.handle_typed(payload, snapshot).await
    }
}

pub(crate) type ScheduledActionHandlerArc = Arc<dyn ErasedTypedScheduledActionHandler>;
pub(crate) type EffectHandlerArc = Arc<dyn ErasedTypedEffectHandler>;
