// Console aliases over the generated management-plane contract. Rust schemars
// output in contracts/model-config.d.ts is the single wire-type authority; this
// module only preserves the concise names already used by the UI.

import type * as Contract from "../../../../contracts/model-config";

export type ApiDialect = Contract.APIDialect;
export type CatalogSyncResult = Contract.CatalogSyncResult;
export type ConfigCapabilitiesView = Contract.ConfigCapabilitiesView;
export type ModelAttributes = Contract.ModelAttributes;
export type ModelTarget = Contract.ModelTarget;
export type Offering = Contract.Offering;
export type ProtocolEndpoint = Contract.ProtocolEndpoint;
export type Provider = Contract.Provider;
export type ProviderCatalog = Contract.ProviderCatalog;
export type ProviderConnectionSummary = Contract.ProviderConnectionSummary;
export type ProviderConnectionView = Contract.ProviderConnectionView;
export type ProviderDriverDescriptor = Contract.ProviderDriverDescriptor;
