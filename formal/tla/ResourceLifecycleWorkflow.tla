---------------------- MODULE ResourceLifecycleWorkflow ----------------------
EXTENDS Naturals

\* Durable prepare-before-I/O activation followed by terminal release and
\* generation-fenced, reference/lease-safe physical reclamation.
CONSTANT MaxGeneration, MaxReferences, MaxLeases
ASSUME
    /\ MaxGeneration \in Nat \ {0}
    /\ MaxReferences \in Nat \ {0}
    /\ MaxLeases \in Nat \ {0}

VARIABLES lifecycle, generation, effectGeneration, activeGeneration,
          references, leases, purgeGeneration, receiptGeneration

vars == <<lifecycle, generation, effectGeneration, activeGeneration,
          references, leases, purgeGeneration, receiptGeneration>>

Init ==
    /\ lifecycle = "Absent"
    /\ generation = 0
    /\ effectGeneration = 0
    /\ activeGeneration = 0
    /\ references = 0
    /\ leases = 0
    /\ purgeGeneration = 0
    /\ receiptGeneration = 0

Prepare ==
    /\ lifecycle = "Absent"
    /\ generation < MaxGeneration
    /\ generation' = generation + 1
    /\ lifecycle' = "Prepared"
    /\ UNCHANGED <<effectGeneration, activeGeneration, references,
                   leases, purgeGeneration, receiptGeneration>>

RecordEffect ==
    /\ lifecycle = "Prepared"
    /\ effectGeneration' = generation
    /\ lifecycle' = "Effected"
    /\ UNCHANGED <<generation, activeGeneration, references,
                   leases, purgeGeneration, receiptGeneration>>

Activate ==
    /\ lifecycle = "Effected"
    /\ effectGeneration = generation
    /\ activeGeneration' = generation
    /\ lifecycle' = "Active"
    /\ UNCHANGED <<generation, effectGeneration, references,
                   leases, purgeGeneration, receiptGeneration>>

AcquireReference ==
    /\ lifecycle = "Active"
    /\ references < MaxReferences
    /\ references' = references + 1
    /\ UNCHANGED <<lifecycle, generation, effectGeneration, activeGeneration,
                   leases, purgeGeneration, receiptGeneration>>

ReleaseReference ==
    /\ references > 0
    /\ references' = references - 1
    /\ UNCHANGED <<lifecycle, generation, effectGeneration, activeGeneration,
                   leases, purgeGeneration, receiptGeneration>>

AcquireLease ==
    /\ lifecycle = "Active"
    /\ leases < MaxLeases
    /\ leases' = leases + 1
    /\ UNCHANGED <<lifecycle, generation, effectGeneration, activeGeneration,
                   references, purgeGeneration, receiptGeneration>>

ReleaseLease ==
    /\ leases > 0
    /\ leases' = leases - 1
    /\ UNCHANGED <<lifecycle, generation, effectGeneration, activeGeneration,
                   references, purgeGeneration, receiptGeneration>>

Terminate ==
    /\ lifecycle = "Active"
    /\ lifecycle' = "Terminated"
    /\ UNCHANGED <<generation, effectGeneration, activeGeneration,
                   references, leases, purgeGeneration, receiptGeneration>>

Purge ==
    /\ lifecycle = "Terminated"
    /\ references = 0
    /\ leases = 0
    /\ purgeGeneration # generation
    /\ purgeGeneration' = generation
    /\ receiptGeneration' = generation
    /\ lifecycle' = "Reclaimed"
    /\ UNCHANGED <<generation, effectGeneration, activeGeneration, references, leases>>

Next ==
    \/ Prepare
    \/ RecordEffect
    \/ Activate
    \/ AcquireReference
    \/ ReleaseReference
    \/ AcquireLease
    \/ ReleaseLease
    \/ Terminate
    \/ Purge

Spec == Init /\ [][Next]_vars

HappyNext == Prepare \/ RecordEffect \/ Activate \/ Terminate \/ Purge
HappySpec ==
    /\ Init
    /\ [][HappyNext]_vars
    /\ WF_vars(Prepare)
    /\ WF_vars(RecordEffect)
    /\ WF_vars(Activate)
    /\ WF_vars(Terminate)
    /\ WF_vars(Purge)

TypeOK ==
    /\ lifecycle \in {"Absent", "Prepared", "Effected", "Active", "Terminated", "Reclaimed"}
    /\ generation \in 0..MaxGeneration
    /\ effectGeneration \in 0..MaxGeneration
    /\ activeGeneration \in 0..MaxGeneration
    /\ references \in 0..MaxReferences
    /\ leases \in 0..MaxLeases
    /\ purgeGeneration \in 0..MaxGeneration
    /\ receiptGeneration \in 0..MaxGeneration

ActivationRequiresEffect == lifecycle = "Active" => activeGeneration = effectGeneration
NoTerminalReactivation == lifecycle \in {"Terminated", "Reclaimed"} => activeGeneration = generation
PurgeRequiresExclusion == receiptGeneration > 0 => references = 0 /\ leases = 0
ReceiptIsGenerationFenced == receiptGeneration > 0 => receiptGeneration = purgeGeneration

Safety ==
    /\ TypeOK
    /\ ActivationRequiresEffect
    /\ NoTerminalReactivation
    /\ PurgeRequiresExclusion
    /\ ReceiptIsGenerationFenced

EventuallyReclaimed == <> (lifecycle = "Reclaimed")
=============================================================================
