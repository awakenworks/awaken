------------------------- MODULE OutcomeLifecycle -------------------------
EXTENDS Naturals, FiniteSets

CONSTANT MaxIterations

Phases == {"defined", "running_worker", "evaluating", "acknowledging",
           "completed", "errored"}
Results == {"none", "satisfied", "failed", "max_iterations_reached",
            "interrupted"}
Grades == {"none", "satisfied", "failed", "needs_revision"}

VARIABLE phase, result, iteration, evaluations, acknowledgments, lastGrade,
         workerRuns, graderRuns, evaluated, version

vars == <<phase, result, iteration, evaluations, acknowledgments, lastGrade,
          workerRuns, graderRuns, evaluated, version>>

Init == /\ MaxIterations > 0
        /\ phase = "defined"
        /\ result = "none"
        /\ iteration = 0
        /\ evaluations = 0
        /\ acknowledgments = 0
        /\ lastGrade = "none"
        /\ workerRuns = {}
        /\ graderRuns = {}
        /\ evaluated = {}
        /\ version = 0

StartWorker ==
    /\ phase = "defined"
    /\ phase' = "running_worker"
    /\ workerRuns' = workerRuns \union {0}
    /\ version' = version + 1
    /\ UNCHANGED <<result, iteration, evaluations, acknowledgments, lastGrade,
                    graderRuns, evaluated>>

WorkerCommitted ==
    /\ phase = "running_worker"
    /\ phase' = "evaluating"
    /\ graderRuns' = graderRuns \union {iteration}
    /\ version' = version + 1
    /\ UNCHANGED <<result, iteration, evaluations, acknowledgments, lastGrade,
                    workerRuns, evaluated>>

GradeSatisfied ==
    /\ phase = "evaluating"
    /\ phase' = "completed"
    /\ result' = "satisfied"
    /\ lastGrade' = "satisfied"
    /\ evaluations' = evaluations + 1
    /\ evaluated' = evaluated \union {iteration}
    /\ version' = version + 1
    /\ UNCHANGED <<iteration, acknowledgments, workerRuns, graderRuns>>

GradeFailed ==
    /\ phase = "evaluating"
    /\ phase' = "completed"
    /\ result' = "failed"
    /\ lastGrade' = "failed"
    /\ evaluations' = evaluations + 1
    /\ evaluated' = evaluated \union {iteration}
    /\ version' = version + 1
    /\ UNCHANGED <<iteration, acknowledgments, workerRuns, graderRuns>>

GradeNeedsRevision ==
    /\ phase = "evaluating"
    /\ iteration + 1 < MaxIterations
    /\ phase' = "running_worker"
    /\ iteration' = iteration + 1
    /\ result' = "none"
    /\ lastGrade' = "needs_revision"
    /\ evaluations' = evaluations + 1
    /\ evaluated' = evaluated \union {iteration}
    /\ workerRuns' = workerRuns \union {iteration + 1}
    /\ version' = version + 1
    /\ UNCHANGED <<acknowledgments, graderRuns>>

GradeExhausted ==
    /\ phase = "evaluating"
    /\ iteration + 1 = MaxIterations
    /\ phase' = "acknowledging"
    /\ result' = "none"
    /\ lastGrade' = "needs_revision"
    /\ evaluations' = evaluations + 1
    /\ evaluated' = evaluated \union {iteration}
    /\ version' = version + 1
    /\ UNCHANGED <<iteration, acknowledgments, workerRuns, graderRuns>>

Acknowledge ==
    /\ phase = "acknowledging"
    /\ phase' = "completed"
    /\ result' = "max_iterations_reached"
    /\ acknowledgments' = acknowledgments + 1
    /\ version' = version + 1
    /\ UNCHANGED <<iteration, evaluations, lastGrade, workerRuns, graderRuns,
                    evaluated>>

Interrupt ==
    /\ phase \in {"defined", "running_worker", "evaluating", "acknowledging"}
    /\ phase' = "completed"
    /\ result' = "interrupted"
    /\ version' = version + 1
    /\ UNCHANGED <<iteration, evaluations, acknowledgments, lastGrade,
                    workerRuns, graderRuns, evaluated>>

InfrastructureFailure ==
    /\ phase \in {"defined", "running_worker", "evaluating", "acknowledging"}
    /\ phase' = "errored"
    /\ result' = "none"
    /\ version' = version + 1
    /\ UNCHANGED <<iteration, evaluations, acknowledgments, lastGrade,
                    workerRuns, graderRuns, evaluated>>

\* A crash, duplicate delivery, or stale compare-and-set is observationally a
\* stutter: it cannot mint another logical Run or advance the aggregate.
ReplayOrStaleCAS == UNCHANGED vars

Next == StartWorker \/ WorkerCommitted \/ GradeSatisfied \/ GradeFailed
        \/ GradeNeedsRevision \/ GradeExhausted \/ Acknowledge \/ Interrupt
        \/ InfrastructureFailure \/ ReplayOrStaleCAS

TypeOK == /\ phase \in Phases
          /\ result \in Results
          /\ lastGrade \in Grades
          /\ iteration \in 0..(MaxIterations - 1)
          /\ evaluations \in 0..MaxIterations
          /\ acknowledgments \in 0..1
          /\ workerRuns \subseteq 0..(MaxIterations - 1)
          /\ graderRuns \subseteq 0..(MaxIterations - 1)
          /\ evaluated \subseteq 0..(MaxIterations - 1)
          /\ version \in Nat

StableRunIdentity == /\ workerRuns = {} \/ workerRuns = 0..iteration
                     /\ graderRuns \subseteq workerRuns
                     /\ evaluated \subseteq graderRuns
                     /\ evaluations = Cardinality(evaluated)

BudgetBounded == evaluations <= MaxIterations /\ iteration < MaxIterations

AcknowledgmentExactlyOnce ==
    /\ (phase = "acknowledging" =>
          evaluations = MaxIterations /\ lastGrade = "needs_revision"
          /\ acknowledgments = 0)
    /\ (result = "max_iterations_reached" =>
          phase = "completed" /\ evaluations = MaxIterations
          /\ lastGrade = "needs_revision" /\ acknowledgments = 1)

TerminalCoherence ==
    /\ (result # "none" => phase = "completed")
    /\ (phase = "errored" => result = "none")
    /\ (phase \in {"defined", "running_worker", "evaluating", "acknowledging"}
          => result = "none")

Spec == Init /\ [][Next]_vars
=============================================================================
