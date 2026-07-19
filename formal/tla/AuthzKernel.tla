--------------------------- MODULE AuthzKernel ---------------------------
EXTENDS Naturals, TLC

CONSTANT Methods

ReadMethods == {"GET", "HEAD"}
SharedDecisions == {"allow", "deny", "approval"}

Action(method) == IF method \in ReadMethods THEN "read" ELSE "write"
Collapse(decision) == IF decision = "allow" THEN "allow" ELSE "deny"

VARIABLE method, sharedDecision

vars == <<method, sharedDecision>>

Init == /\ method \in Methods
        /\ sharedDecision \in SharedDecisions

ChooseMethod(m) == /\ m \in Methods
                   /\ method' = m
                   /\ UNCHANGED sharedDecision

ChooseDecision(d) == /\ d \in SharedDecisions
                     /\ sharedDecision' = d
                     /\ UNCHANGED method

Next == (\E m \in Methods: ChooseMethod(m))
     \/ (\E d \in SharedDecisions: ChooseDecision(d))

TypeOK == /\ method \in Methods
          /\ sharedDecision \in SharedDecisions

MethodMappingTotal == Action(method) \in {"read", "write"}
OnlySafeMethodsRead == (Action(method) = "read") <=> (method \in ReadMethods)
DecisionCollapseFailClosed ==
    (Collapse(sharedDecision) = "allow") <=> (sharedDecision = "allow")

Spec == Init /\ [][Next]_vars
=============================================================================
