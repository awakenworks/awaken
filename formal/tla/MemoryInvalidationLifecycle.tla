-------------------- MODULE MemoryInvalidationLifecycle --------------------
EXTENDS Naturals, TLC

\* The projection-local listener owner, NOT Memory CAS or Provider compensation.
\* Two projections expose cross-owner interference. Setup failure/unwind and
\* handle unmount/Drop share Stop; only the owner can join and return. A thread
\* may exit before Stop (closed bus/panic). Cancellation is not thread exit.
\* Fair worker scheduling and a terminating cache critical section are assumed;
\* abort, Worker death, kernel unmount and Rust refinement are outside this model.
Owners == 1..2
VARIABLES phase, task, stop, joined, admitted
vars == <<phase, task, stop, joined, admitted>>

Init ==
    /\ phase = [i \in Owners |-> "New"]
    /\ task = [i \in Owners |-> "Absent"]
    /\ stop = [i \in Owners |-> FALSE]
    /\ joined = [i \in Owners |-> FALSE]
    /\ admitted = [i \in Owners |-> FALSE]

Start(i) ==
    /\ phase[i] = "New"
    /\ phase' = [phase EXCEPT ![i] = "Mounting"]
    /\ task' = [task EXCEPT ![i] = "Running"]
    /\ UNCHANGED <<stop, joined, admitted>>

Admit(i) ==
    /\ phase[i] = "Mounting"
    /\ phase' = [phase EXCEPT ![i] = "Active"]
    /\ admitted' = [admitted EXCEPT ![i] = TRUE]
    /\ UNCHANGED <<task, stop, joined>>

Stop(i) ==
    /\ phase[i] \in {"Mounting", "Active"}
    /\ phase' = [phase EXCEPT ![i] = "Stopping"]
    /\ stop' = [stop EXCEPT ![i] = TRUE]
    /\ UNCHANGED <<task, joined, admitted>>

Exit(i) ==
    /\ task[i] = "Running" /\ stop[i]
    /\ task' = [task EXCEPT ![i] = "Exited"]
    /\ UNCHANGED <<phase, stop, joined, admitted>>

EarlyExit(i) ==
    /\ task[i] = "Running" /\ ~stop[i]
    /\ task' = [task EXCEPT ![i] = "Exited"]
    /\ UNCHANGED <<phase, stop, joined, admitted>>

Finish(i) ==
    /\ phase[i] = "Stopping"
    /\ task[i] = "Exited"
    /\ phase' = [phase EXCEPT ![i] = "Done"]
    /\ joined' = [joined EXCEPT ![i] = TRUE]
    /\ UNCHANGED <<task, stop, admitted>>

Next == \E i \in Owners: Start(i) \/ Admit(i) \/ Stop(i)
                            \/ Exit(i) \/ EarlyExit(i) \/ Finish(i)
Spec == Init /\ [][Next]_vars
        /\ (\A i \in Owners: WF_vars(Exit(i)) /\ WF_vars(Finish(i)))

TypeOK ==
    /\ phase \in [Owners -> {"New", "Mounting", "Active", "Stopping", "Done"}]
    /\ task \in [Owners -> {"Absent", "Running", "Exited"}]
    /\ stop \in [Owners -> BOOLEAN]
    /\ joined \in [Owners -> BOOLEAN]
    /\ admitted \in [Owners -> BOOLEAN]
StoppingRequestsStop == \A i \in Owners: phase[i] = "Stopping" => stop[i]
NoPeerStop == \A i \in Owners: stop[i] => phase[i] \in {"Stopping", "Done"}
ReturnRequiresJoin == \A i \in Owners:
    phase[i] = "Done" => (task[i] = "Exited" /\ joined[i])
CleanupEventuallyReturns == \A i \in Owners: (phase[i] = "Stopping") ~> (phase[i] = "Done")

\* Negated reachability oracles: the checker requires these exact failures.
NoFailedSetupReturned == ~\E i \in Owners: phase[i] = "Done" /\ ~admitted[i]
NoSuccessfulMountReturned == ~\E i \in Owners: phase[i] = "Done" /\ admitted[i]
NoPeerMountsActive == ~(\A i \in Owners: phase[i] = "Active")
NoEarlyExitReached == ~\E i \in Owners: task[i] = "Exited" /\ ~stop[i]
=============================================================================
