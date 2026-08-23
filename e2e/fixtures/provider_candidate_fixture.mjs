// Raw native Provider candidate shared by wire and durable-state E2Es. Product
// validity remains owned by ResolvedModelCandidate::try_from_parts during typed
// decoding: this helper only removes repeated JSON assembly and deliberately
// supplies no defaults, inference, compatibility decisions, or validation.

/**
 * @param {{
 *   binding: Record<string, unknown>,
 *   providerRef: string,
 *   routeRef: string,
 *   scopeId: string,
 *   credential: unknown,
 *   adapterKind: string,
 *   apiDialect: string,
 *   baseUrl: string,
 *   upstreamModel: string,
 * }} input
 */
export function nativeProviderCandidateFixture({
  binding,
  providerRef,
  routeRef,
  scopeId,
  credential,
  adapterKind,
  apiDialect,
  baseUrl,
  upstreamModel,
}) {
  return {
    ...structuredClone(binding),
    provisioning: {
      type: 'provider',
      provider_ref: providerRef,
      route_ref: routeRef,
      scope_id: scopeId,
      credential: structuredClone(credential),
      endpoint: {
        adapter_kind: adapterKind,
        api_dialect: apiDialect,
        base_url: baseUrl,
        upstream_model: upstreamModel,
      },
    },
  };
}
