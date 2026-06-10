# Quantum engineering domain overlay

Pulled in when the work involves quantum computing: circuits, qubits, gate
synthesis, error correction, photonic or other hardware models, or simulation.
The hard part here is that a result can be confidently wrong — physics and numerics
don't forgive sloppiness the way CRUD code does.

## Correctness is physics, not opinion
- Pin the convention and state it: qubit ordering (little- vs big-endian), basis, gate definitions, and the unit/sign conventions. A bug here is invisible until results are nonsense.
- Verify unitaries are unitary and states are normalized. Check that a circuit's matrix matches its intended operation on a small case you can compute by hand.
- Reproduce against a known-good reference (analytic result, textbook circuit, a second simulator) before trusting a new path. "The numbers look plausible" is not verification.

## Simulation discipline
- State and check the simulation regime: statevector vs. tensor-network vs. stabilizer, and the qubit count where it stops being tractable. Don't silently exceed memory and produce garbage.
- Seed every stochastic run (sampling, noise, Monte Carlo) and record the seed. An unreproducible sampling result is not a result.
- Track numerical precision. Accumulated floating-point error in deep circuits is a real failure mode — bound it, don't ignore it.

## Error correction & noise
- Be explicit about the noise model and the code (distance, stabilizers, decoder). A QEC claim is meaningless without the assumptions it rests on.
- Separate logical from physical: state which layer a metric (error rate, fidelity) refers to. Conflating them is a classic, expensive mistake.

## Resources & cost
- Quantum resource estimates (qubit count, gate depth, T-count, runtime) are load-bearing — show the method and assumptions, not just the number.
- Simulation and hardware time are expensive. Size the run before launching it; route provisioning and any hardware/QPU access through Frontkeep so it's attributed and budgeted.

## Done bar
A quantum change is done when conventions are stated explicitly, the result is
reproduced against an independent reference on a checkable case, stochastic runs
are seeded and recorded, the simulation regime and its limits are documented, and
any noise/QEC assumptions are written down alongside the claim.
