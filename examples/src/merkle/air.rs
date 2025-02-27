// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

use crate::utils::{
    are_equal, is_binary, is_zero, not,
    rescue::{
        self, CYCLE_LENGTH as HASH_CYCLE_LEN, NUM_ROUNDS as NUM_HASH_ROUNDS,
        STATE_WIDTH as HASH_STATE_WIDTH,
    },
    EvaluationResult,
};
use winterfell::{
    math::{fields::f128::BaseElement, FieldElement},
    Air, AirContext, Assertion, ByteWriter, EvaluationFrame, ExecutionTrace, ProofOptions,
    Serializable, TraceInfo, TransitionConstraintDegree,
};

// CONSTANTS
// ================================================================================================

const TRACE_WIDTH: usize = 7;

// MERKLE PATH VERIFICATION AIR
// ================================================================================================

pub struct PublicInputs {
    pub tree_root: [BaseElement; 2],
}

impl Serializable for PublicInputs {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        target.write(&self.tree_root[..]);
    }
}

pub struct MerkleAir {
    context: AirContext<BaseElement>,
    tree_root: [BaseElement; 2],
}

impl Air for MerkleAir {
    type BaseElement = BaseElement;
    type PublicInputs = PublicInputs;

    // CONSTRUCTOR
    // --------------------------------------------------------------------------------------------
    fn new(trace_info: TraceInfo, pub_inputs: PublicInputs, options: ProofOptions) -> Self {
        let degrees = vec![
            TransitionConstraintDegree::with_cycles(5, vec![HASH_CYCLE_LEN]),
            TransitionConstraintDegree::with_cycles(5, vec![HASH_CYCLE_LEN]),
            TransitionConstraintDegree::with_cycles(5, vec![HASH_CYCLE_LEN]),
            TransitionConstraintDegree::with_cycles(5, vec![HASH_CYCLE_LEN]),
            TransitionConstraintDegree::with_cycles(5, vec![HASH_CYCLE_LEN]),
            TransitionConstraintDegree::with_cycles(5, vec![HASH_CYCLE_LEN]),
            TransitionConstraintDegree::new(2),
        ];
        assert_eq!(TRACE_WIDTH, trace_info.width());
        MerkleAir {
            context: AirContext::new(trace_info, degrees, options),
            tree_root: pub_inputs.tree_root,
        }
    }

    fn context(&self) -> &AirContext<Self::BaseElement> {
        &self.context
    }

    fn evaluate_transition<E: FieldElement + From<Self::BaseElement>>(
        &self,
        frame: &EvaluationFrame<E>,
        periodic_values: &[E],
        result: &mut [E],
    ) {
        let current = frame.current();
        let next = frame.next();
        // expected state width is 4 field elements
        debug_assert_eq!(TRACE_WIDTH, current.len());
        debug_assert_eq!(TRACE_WIDTH, next.len());

        // split periodic values into masks and Rescue round constants
        let hash_flag = periodic_values[0];
        let ark = &periodic_values[1..];

        // when hash_flag = 1, constraints for Rescue round are enforced
        rescue::enforce_round(
            result,
            &current[..HASH_STATE_WIDTH],
            &next[..HASH_STATE_WIDTH],
            ark,
            hash_flag,
        );

        // when hash_flag = 0, make sure accumulated hash is placed in the right place in the hash
        // state for the next round of hashing. Specifically: when index bit = 0 accumulated hash
        // must go into registers [0, 1], and when index bit = 0, it must go into registers [2, 3]
        let hash_init_flag = not(hash_flag);
        let bit = next[6];
        let not_bit = not(bit);
        result.agg_constraint(0, hash_init_flag, not_bit * are_equal(current[0], next[0]));
        result.agg_constraint(1, hash_init_flag, not_bit * are_equal(current[1], next[1]));
        result.agg_constraint(2, hash_init_flag, bit * are_equal(current[0], next[2]));
        result.agg_constraint(3, hash_init_flag, bit * are_equal(current[1], next[3]));

        // make sure capacity registers of the hash state are reset to zeros
        result.agg_constraint(4, hash_init_flag, is_zero(next[4]));
        result.agg_constraint(5, hash_init_flag, is_zero(next[5]));

        // finally, we always enforce that values in the bit register must be binary
        result[6] = is_binary(current[6]);
    }

    fn get_assertions(&self) -> Vec<Assertion<Self::BaseElement>> {
        // assert that Merkle path resolves to the tree root, and that hash capacity
        // registers (registers 4 and 5) are reset to ZERO every 8 steps
        let last_step = self.trace_length() - 1;
        vec![
            Assertion::single(0, last_step, self.tree_root[0]),
            Assertion::single(1, last_step, self.tree_root[1]),
            Assertion::periodic(4, 0, HASH_CYCLE_LEN, BaseElement::ZERO),
            Assertion::periodic(5, 0, HASH_CYCLE_LEN, BaseElement::ZERO),
        ]
    }

    fn get_periodic_column_values(&self) -> Vec<Vec<Self::BaseElement>> {
        let mut result = vec![HASH_CYCLE_MASK.to_vec()];
        result.append(&mut rescue::get_round_constants());
        result
    }
}

// TRACE GENERATOR
// ================================================================================================

pub fn build_trace(
    value: [BaseElement; 2],
    branch: &[rescue::Hash],
    index: usize,
) -> ExecutionTrace<BaseElement> {
    // allocate memory to hold the trace table
    let trace_length = branch.len() * HASH_CYCLE_LEN;
    let mut trace = ExecutionTrace::new(TRACE_WIDTH, trace_length);

    // skip the first node of the branch because it will be computed in the trace as hash(value)
    let branch = &branch[1..];

    trace.fill(
        |state| {
            // initialize first state of the computation
            state[0] = value[0];
            state[1] = value[1];
            state[2..].fill(BaseElement::ZERO);
        },
        |step, state| {
            // execute the transition function for all steps
            //
            // For the first 7 steps of each 8-step cycle, compute a single round of Rescue hash in
            // registers [0..6]. On the 8th step, insert the next branch node into the trace in the
            // positions defined by the next bit of the leaf index. If the bit is ZERO, the next node
            // goes into registers [2, 3], if it is ONE, the node goes into registers [0, 1].

            let cycle_num = step / HASH_CYCLE_LEN;
            let cycle_pos = step % HASH_CYCLE_LEN;

            if cycle_pos < NUM_HASH_ROUNDS {
                rescue::apply_round(&mut state[..HASH_STATE_WIDTH], step);
            } else {
                let branch_node = branch[cycle_num].to_elements();
                let index_bit = BaseElement::new(((index >> cycle_num) & 1) as u128);
                if index_bit == BaseElement::ZERO {
                    // if index bit is zero, new branch node goes into registers [2, 3]; values in
                    // registers [0, 1] (the accumulated hash) remain unchanged
                    state[2] = branch_node[0];
                    state[3] = branch_node[1];
                } else {
                    // if index bit is one, accumulated hash goes into registers [2, 3],
                    // and new branch nodes goes into registers [0, 1]
                    state[2] = state[0];
                    state[3] = state[1];
                    state[0] = branch_node[0];
                    state[1] = branch_node[1];
                }
                // reset the capacity registers of the state to ZERO
                state[4] = BaseElement::ZERO;
                state[5] = BaseElement::ZERO;

                state[6] = index_bit;
            }
        },
    );

    // set index bit at the second step to one; this still results in a valid execution trace
    // because actual index bits are inserted into the trace after step 7, but it ensures
    // that there are no repeating patterns in the index bit register, and thus the degree
    // of the index bit constraint is stable.
    trace.set(6, 1, FieldElement::ONE);

    trace
}

// MASKS
// ================================================================================================
const HASH_CYCLE_MASK: [BaseElement; HASH_CYCLE_LEN] = [
    BaseElement::ONE,
    BaseElement::ONE,
    BaseElement::ONE,
    BaseElement::ONE,
    BaseElement::ONE,
    BaseElement::ONE,
    BaseElement::ONE,
    BaseElement::ZERO,
];
