import type * as Contract from "../../../../contracts/model-config";
export type McpDefinition = Contract.McpServerDefinition;
export type McpServerRow = Contract.McpServerView;
export type McpConfigurationView = Contract.McpConfigurationView;
export type McpCredentialChoice = Contract.McpCredentialChoice;
export type McpPreparedBinding = Contract.McpPreparedBinding;
export type McpInspection = Contract.McpInspectionView;
export function mcpAddress(definition: McpDefinition): string {
  return definition.transport.type === "url" ? definition.transport.url
    : [definition.transport.command, ...(definition.transport.args ?? [])].join(" ");
}
