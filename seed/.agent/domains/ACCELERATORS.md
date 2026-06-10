# Hardware accelerators domain overlay

Pulled in when the work targets GPUs, FPGAs, ASICs, or other accelerators —
kernels, CUDA, HLS, RTL, SIMD. Correctness and performance are coupled here: the
fast path and the right answer are often in tension, and the hardware will let you
be subtly wrong at full speed.

## Correctness first, then speed
- Validate the accelerated path against a simple, obviously-correct reference (a CPU/scalar implementation) on representative and edge-case inputs before optimizing. A fast kernel that's wrong is worthless.
- Race conditions are the default failure: guard shared memory, get synchronization barriers right, and don't assume execution order across threads/lanes. Test with a race detector where one exists.
- Watch numerics: reduced/mixed precision, non-associative floating-point reductions, and fast-math flags change results. State the precision contract and verify tolerances explicitly.

## Memory & data movement
- The transfer is usually the bottleneck, not the compute. Account for host↔device movement; don't optimize a kernel while leaving the data ping-ponging across the bus.
- Respect the memory hierarchy (registers, shared/local, global): coalesce accesses, avoid bank conflicts, and bound on-chip resource usage so occupancy doesn't collapse.
- Bound-check at the boundary. Out-of-range indexing on a device often corrupts silently instead of faulting.

## Measure, don't guess
- Profile before and after with the real tool, on the target hardware, with realistic input sizes. A microbenchmark on a toy input lies. Report the speedup with the baseline and the conditions.
- State the hardware target and its assumptions (compute capability, board, clock). Code tuned for one device can regress on another.

## Resources & cost
- Accelerator time is expensive and contended. Size and budget runs before launching; route hardware provisioning through Frontkeep so it's attributed and capped.

## Done bar
An accelerator change is done when the result is validated against a reference
implementation, synchronization is correct (race-checked where possible), the
precision contract is stated and tolerances verified, performance is measured on
the target hardware with the baseline reported, and host/device data movement is
accounted for — not just kernel time.
