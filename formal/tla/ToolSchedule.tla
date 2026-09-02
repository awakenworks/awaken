----------------------------- MODULE ToolSchedule -----------------------------
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS CallCount, Parallel, Serial, ReadA, WriteA, ReadB, WriteB

ClaimKinds == {Parallel, Serial, ReadA, WriteA, ReadB, WriteB}

ASSUME /\ CallCount > 0
       /\ Cardinality(ClaimKinds) = 6

IsSerial(claim) == claim = Serial

IsWrite(claim) == claim \in {WriteA, WriteB}

SameResource(left, right) ==
    \/ /\ left \in {ReadA, WriteA}
       /\ right \in {ReadA, WriteA}
    \/ /\ left \in {ReadB, WriteB}
       /\ right \in {ReadB, WriteB}

Compatible(left, right) ==
    IF IsSerial(left) \/ IsSerial(right)
    THEN FALSE
    ELSE IF left = Parallel \/ right = Parallel
         THEN TRUE
         ELSE ~(SameResource(left, right) /\ (IsWrite(left) \/ IsWrite(right)))

VARIABLES claims, limit, cursor, openWave, closedWaves

vars == <<claims, limit, cursor, openWave, closedWaves>>

RECURSIVE Flatten(_)
Flatten(waves) ==
    IF Len(waves) = 0
    THEN <<>>
    ELSE Head(waves) \o Flatten(Tail(waves))

AllWaves ==
    IF Len(openWave) = 0
    THEN closedWaves
    ELSE Append(closedWaves, openWave)

Scheduled == Flatten(closedWaves) \o openWave

Init ==
    /\ claims \in [1..CallCount -> ClaimKinds]
    /\ limit \in 1..CallCount
    /\ cursor = 1
    /\ openWave = <<>>
    /\ closedWaves = <<>>

CanExtend ==
    /\ cursor <= CallCount
    /\ Len(openWave) > 0
    /\ ~IsSerial(claims[Head(openWave)])
    /\ Len(openWave) < limit
    /\ \A position \in 1..Len(openWave) :
         Compatible(claims[openWave[position]], claims[cursor])

StartWave ==
    /\ cursor <= CallCount
    /\ Len(openWave) = 0
    /\ openWave' = <<cursor>>
    /\ cursor' = cursor + 1
    /\ UNCHANGED <<claims, limit, closedWaves>>

ExtendWave ==
    /\ CanExtend
    /\ openWave' = Append(openWave, cursor)
    /\ cursor' = cursor + 1
    /\ UNCHANGED <<claims, limit, closedWaves>>

CloseWave ==
    /\ Len(openWave) > 0
    /\ (cursor > CallCount \/ ~CanExtend)
    /\ closedWaves' = Append(closedWaves, openWave)
    /\ openWave' = <<>>
    /\ UNCHANGED <<claims, limit, cursor>>

Next == StartWave \/ ExtendWave \/ CloseWave

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ claims \in [1..CallCount -> ClaimKinds]
    /\ limit \in 1..CallCount
    /\ cursor \in 1..(CallCount + 1)
    /\ openWave \in Seq(1..CallCount)
    /\ closedWaves \in Seq(Seq(1..CallCount))

StableOrderIsOneExactPrefix ==
    Scheduled = [index \in 1..(cursor - 1) |-> index]

EveryWaveIsNonemptyAndBounded ==
    \A waveIndex \in 1..Len(AllWaves) :
      /\ Len(AllWaves[waveIndex]) >= 1
      /\ Len(AllWaves[waveIndex]) <= limit

EveryWaveIsPairwiseCompatible ==
    \A waveIndex \in 1..Len(AllWaves) :
      \A left, right \in 1..Len(AllWaves[waveIndex]) :
        left < right =>
          Compatible(
            claims[AllWaves[waveIndex][left]],
            claims[AllWaves[waveIndex][right]])

SerialClaimsAreSingletonWaves ==
    \A waveIndex \in 1..Len(AllWaves) :
      IsSerial(claims[Head(AllWaves[waveIndex])]) =>
        Len(AllWaves[waveIndex]) = 1

ClosedWavesAreMaximalStablePrefixes ==
    \A waveIndex \in 1..Len(closedWaves) :
      LET wave == closedWaves[waveIndex]
          next == wave[Len(wave)] + 1
      IN next > CallCount
         \/ Len(wave) = limit
         \/ IsSerial(claims[Head(wave)])
         \/ \E position \in 1..Len(wave) :
              ~Compatible(claims[wave[position]], claims[next])

TerminalScheduleContainsEveryCallExactlyOnce ==
    (cursor = CallCount + 1 /\ Len(openWave) = 0) =>
      Flatten(closedWaves) = [index \in 1..CallCount |-> index]

=============================================================================
