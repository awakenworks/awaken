------------------------ MODULE ToolPermissionPolicy ------------------------
EXTENDS TLC

CONSTANTS Tools, AgentTools, McpTools, ExtensionTools

VARIABLES configuration, decision

vars == <<configuration, decision>>
Configurations == {"inherit", "always_allow", "always_ask", "disabled"}
Decisions == {"allow", "ask", "deny"}

DefaultDecision(tool) == IF tool \in AgentTools THEN "allow" ELSE "ask"
ExpectedDecision(tool, configured) ==
    CASE configured = "disabled" -> "deny"
      [] configured = "always_allow" -> "allow"
      [] configured = "always_ask" -> "ask"
      [] OTHER -> DefaultDecision(tool)

Init ==
    /\ AgentTools \cup McpTools \cup ExtensionTools = Tools
    /\ AgentTools \intersect McpTools = {}
    /\ AgentTools \intersect ExtensionTools = {}
    /\ McpTools \intersect ExtensionTools = {}
    /\ configuration = [tool \in Tools |-> "inherit"]
    /\ decision = [tool \in Tools |-> DefaultDecision(tool)]

Configure(tool, configured) ==
    /\ tool \in Tools
    /\ configured \in Configurations
    /\ configuration' = [configuration EXCEPT ![tool] = configured]
    /\ decision' = [decision EXCEPT ![tool] = ExpectedDecision(tool, configured)]

Next == \E tool \in Tools, configured \in Configurations: Configure(tool, configured)
Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ configuration \in [Tools -> Configurations]
    /\ decision \in [Tools -> Decisions]

ManagedDefaultsAreSourceExact ==
    \A tool \in Tools:
        configuration[tool] = "inherit" =>
            IF tool \in AgentTools
            THEN decision[tool] = "allow"
            ELSE decision[tool] = "ask"

ExactConfigurationIsAuthoritative ==
    \A tool \in Tools:
        decision[tool] = ExpectedDecision(tool, configuration[tool])

DisabledNeverExecutes ==
    \A tool \in Tools: configuration[tool] = "disabled" => decision[tool] = "deny"

Safety ==
    /\ TypeOK
    /\ ManagedDefaultsAreSourceExact
    /\ ExactConfigurationIsAuthoritative
    /\ DisabledNeverExecutes
=============================================================================
