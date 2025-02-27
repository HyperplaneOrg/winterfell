// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

use super::{CompositionPoly, ProverError, StarkDomain};
use air::ConstraintDivisor;
use math::{batch_inversion, fft, FieldElement, StarkField};
use utils::{batch_iter_mut, collections::Vec, iter_mut, uninit_vector};

#[cfg(feature = "concurrent")]
use utils::iterators::*;

#[cfg(not(debug_assertions))]
use core::marker::PhantomData;

// CONSTANTS
// ================================================================================================

const MIN_FRAGMENT_SIZE: usize = 16;

// CONSTRAINT EVALUATION TABLE
// ================================================================================================

pub struct ConstraintEvaluationTable<B: StarkField, E: FieldElement<BaseField = B>> {
    evaluations: Vec<Vec<E>>,
    divisors: Vec<ConstraintDivisor<B>>,
    domain_offset: B,
    trace_length: usize,

    #[cfg(debug_assertions)]
    t_evaluations: Vec<Vec<B>>,
    #[cfg(debug_assertions)]
    t_expected_degrees: Vec<usize>,
}

impl<B: StarkField, E: FieldElement<BaseField = B>> ConstraintEvaluationTable<B, E> {
    // CONSTRUCTOR
    // --------------------------------------------------------------------------------------------
    /// Returns a new constraint evaluation table with number of columns equal to the number of
    /// specified divisors, and number of rows equal to the size of constraint evaluation domain.
    #[cfg(not(debug_assertions))]
    pub fn new(domain: &StarkDomain<B>, divisors: Vec<ConstraintDivisor<B>>) -> Self {
        let num_columns = divisors.len();
        let num_rows = domain.ce_domain_size();
        ConstraintEvaluationTable {
            evaluations: unsafe { (0..num_columns).map(|_| uninit_vector(num_rows)).collect() },
            divisors,
            domain_offset: domain.offset(),
            trace_length: domain.trace_length(),
        }
    }

    /// Similar to the as above constructor but used in debug mode. In debug mode we also want
    /// to keep track of all evaluated transition constraints so that we can verify that their
    /// expected degrees match their actual degrees.
    #[cfg(debug_assertions)]
    pub fn new(
        domain: &StarkDomain<B>,
        divisors: Vec<ConstraintDivisor<B>>,
        transition_constraint_degrees: Vec<usize>,
    ) -> Self {
        let num_columns = divisors.len();
        let num_rows = domain.ce_domain_size();
        let num_t_columns = transition_constraint_degrees.len();
        ConstraintEvaluationTable {
            evaluations: unsafe { (0..num_columns).map(|_| uninit_vector(num_rows)).collect() },
            divisors,
            domain_offset: domain.offset(),
            trace_length: domain.trace_length(),
            t_evaluations: unsafe {
                (0..num_t_columns)
                    .map(|_| uninit_vector(num_rows))
                    .collect()
            },
            t_expected_degrees: transition_constraint_degrees,
        }
    }

    // PUBLIC ACCESSORS
    // --------------------------------------------------------------------------------------------

    /// Returns the number of rows in this table. This is the same as the size of the constraint
    /// evaluation domain.
    pub fn num_rows(&self) -> usize {
        self.evaluations[0].len()
    }

    /// Returns number of columns in this table. The first column always contains the value of
    /// combined transition constraint evaluations; the remaining columns contain values of
    /// assertion constraint evaluations combined based on common divisors.
    #[allow(dead_code)]
    pub fn num_columns(&self) -> usize {
        self.evaluations.len()
    }

    // TABLE FRAGMENTS
    // --------------------------------------------------------------------------------------------

    /// Break the table into the number of specified fragments. All fragments can be updated
    /// independently - e.g. in different threads.
    pub fn fragments(&mut self, num_fragments: usize) -> Vec<EvaluationTableFragment<B, E>> {
        let fragment_size = self.num_rows() / num_fragments;
        assert!(
            fragment_size >= MIN_FRAGMENT_SIZE,
            "fragment size must be at least {}, but was {}",
            MIN_FRAGMENT_SIZE,
            fragment_size
        );

        // break evaluations into fragments
        let mut evaluation_data = (0..num_fragments).map(|_| Vec::new()).collect::<Vec<_>>();
        self.evaluations.iter_mut().for_each(|column| {
            for (i, fragment) in column.chunks_mut(fragment_size).enumerate() {
                evaluation_data[i].push(fragment);
            }
        });

        #[cfg(debug_assertions)]
        let result = {
            // in debug mode, also break individual transition evaluations into fragments
            let mut t_evaluation_data = (0..num_fragments).map(|_| Vec::new()).collect::<Vec<_>>();
            self.t_evaluations.iter_mut().for_each(|column| {
                for (i, fragment) in column.chunks_mut(fragment_size).enumerate() {
                    t_evaluation_data[i].push(fragment);
                }
            });

            evaluation_data
                .into_iter()
                .zip(t_evaluation_data)
                .enumerate()
                .map(
                    |(i, (evaluations, t_evaluations))| EvaluationTableFragment {
                        offset: i * fragment_size,
                        evaluations,
                        t_evaluations,
                    },
                )
                .collect()
        };

        #[cfg(not(debug_assertions))]
        let result = {
            evaluation_data
                .into_iter()
                .enumerate()
                .map(|(i, evaluations)| EvaluationTableFragment {
                    offset: i * fragment_size,
                    evaluations,
                    _base_field: PhantomData,
                })
                .collect()
        };

        result
    }

    // CONSTRAINT COMPOSITION
    // --------------------------------------------------------------------------------------------
    /// Divides constraint evaluation columns by their respective divisor (in evaluation form),
    /// combines the results into a single column, and interpolates this column into a composition
    /// polynomial in coefficient form.
    pub fn into_poly(self) -> Result<CompositionPoly<B, E>, ProverError> {
        let domain_offset = self.domain_offset;

        // allocate memory for the combined polynomial
        let mut combined_poly = E::zeroed_vector(self.num_rows());

        // iterate over all columns of the constraint evaluation table, divide each column
        // by the evaluations of its corresponding divisor, and add all resulting evaluations
        // together into a single vector
        for (column, divisor) in self.evaluations.into_iter().zip(self.divisors.iter()) {
            // in debug mode, make sure post-division degree of each column matches the expected
            // degree
            #[cfg(debug_assertions)]
            validate_column_degree(&column, divisor, domain_offset, column.len() - 1)?;

            // divide the column by the divisor and accumulate the result into combined_poly
            acc_column(column, divisor, self.domain_offset, &mut combined_poly);
        }

        // at this point, combined_poly contains evaluations of the combined constraint polynomial;
        // we interpolate this polynomial to transform it into coefficient form.
        let inv_twiddles = fft::get_inv_twiddles::<B>(combined_poly.len());
        fft::interpolate_poly_with_offset(&mut combined_poly, &inv_twiddles, domain_offset);

        Ok(CompositionPoly::new(combined_poly, self.trace_length))
    }

    // DEBUG HELPERS
    // --------------------------------------------------------------------------------------------

    #[cfg(debug_assertions)]
    pub fn validate_transition_degrees(&mut self) {
        // collect actual degrees for all transition constraints by interpolating saved
        // constraint evaluations into polynomials and checking their degree; also
        // determine max transition constraint degree
        let mut actual_degrees = Vec::with_capacity(self.t_expected_degrees.len());
        let mut max_degree = 0;
        let inv_twiddles = fft::get_inv_twiddles::<B>(self.num_rows());
        for evaluations in self.t_evaluations.iter() {
            let mut poly = evaluations.clone();
            fft::interpolate_poly(&mut poly, &inv_twiddles);
            let degree = math::polynom::degree_of(&poly);
            actual_degrees.push(degree);

            max_degree = core::cmp::max(max_degree, degree);
        }

        // make sure expected and actual degrees are equal
        if self.t_expected_degrees != actual_degrees {
            panic!(
                "transition constraint degrees didn't match\nexpected: {:>3?}\nactual:   {:>3?}",
                self.t_expected_degrees, actual_degrees
            );
        }

        // make sure evaluation domain size does not exceed the size required by max degree
        let expected_domain_size =
            core::cmp::max(max_degree, self.trace_length + 1).next_power_of_two();
        if expected_domain_size != self.num_rows() {
            panic!(
                "incorrect constraint evaluation domain size; expected {}, actual: {}",
                expected_domain_size,
                self.num_rows()
            );
        }
    }
}

// TABLE FRAGMENTS
// ================================================================================================

pub struct EvaluationTableFragment<'a, B: StarkField, E: FieldElement<BaseField = B>> {
    offset: usize,
    evaluations: Vec<&'a mut [E]>,

    #[cfg(debug_assertions)]
    t_evaluations: Vec<&'a mut [B]>,

    #[cfg(not(debug_assertions))]
    _base_field: PhantomData<B>,
}

impl<'a, B: StarkField, E: FieldElement<BaseField = B>> EvaluationTableFragment<'a, B, E> {
    /// Returns the row at which the fragment starts.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Returns the number of evaluation rows in the fragment.
    pub fn num_rows(&self) -> usize {
        self.evaluations[0].len()
    }

    /// Returns the number of columns in every evaluation row.
    pub fn num_columns(&self) -> usize {
        self.evaluations.len()
    }

    /// Updates a single row in the fragment with provided data.
    pub fn update_row(&mut self, row_idx: usize, row_data: &[E]) {
        for (column, &value) in self.evaluations.iter_mut().zip(row_data) {
            column[row_idx] = value;
        }
    }

    /// Updates transition evaluations row with the provided data; available only in debug mode.
    #[cfg(debug_assertions)]
    pub fn update_transition_evaluations(&mut self, row_idx: usize, row_data: &[B]) {
        for (column, &value) in self.t_evaluations.iter_mut().zip(row_data) {
            column[row_idx] = value;
        }
    }
}

// HELPER FUNCTIONS
// ================================================================================================

#[allow(clippy::many_single_char_names)]
fn acc_column<B: StarkField, E: FieldElement<BaseField = B>>(
    column: Vec<E>,
    divisor: &ConstraintDivisor<B>,
    domain_offset: B,
    result: &mut [E],
) {
    let numerator = divisor.numerator();
    assert_eq!(numerator.len(), 1, "complex divisors are not yet supported");
    assert!(
        divisor.exclude().len() <= 1,
        "multiple exclusion points are not yet supported"
    );

    // compute inverse evaluations of the divisor's numerator, which has the form (x^a - b)
    let domain_size = column.len();
    let z = get_inv_evaluation(divisor, domain_size, domain_offset);

    // divide column values by the divisor; for boundary constraints this computed simply as
    // multiplication of column value by the inverse of divisor numerator; for transition
    // constraints, it is computed similarly, but the result is also multiplied by the divisor's
    // denominator (exclusion point).
    if divisor.exclude().is_empty() {
        // the column represents merged evaluations of boundary constraints, and divisor has the
        // form of (x^a - b); thus to divide the column by the divisor, we compute: value * z,
        // where z = 1 / (x^a - 1) and has already been computed above.
        iter_mut!(result, 1024)
            .zip(column)
            .enumerate()
            .for_each(|(i, (acc_value, value))| {
                // determine which value of z corresponds to the current domain point
                let z = E::from(z[i % z.len()]);
                // compute value * z and add it to the result
                *acc_value += value * z;
            });
    } else {
        // the column represents merged evaluations of transition constraints, and divisor has the
        // form of (x^a - 1) / (x - b); thus, to divide the column by the divisor, we compute:
        // value * (x - b) * z, where z = 1 / (x^a - 1) and has already been computed above.

        // set up variables for computing x at every point in the domain
        let g = B::get_root_of_unity(domain_size.trailing_zeros());
        let b = divisor.exclude()[0];

        batch_iter_mut!(
            result,
            128, // min batch size
            |batch: &mut [E], batch_offset: usize| {
                let mut x = domain_offset * g.exp((batch_offset as u64).into());
                for (i, acc_value) in batch.iter_mut().enumerate() {
                    // compute value of (x - b) and compute next value of x
                    let e = x - b;
                    x *= g;
                    // determine which value of z corresponds to the current domain point
                    let z = z[i % z.len()];
                    // compute value * (x - b) * z and add it to the result
                    *acc_value += column[batch_offset + i] * E::from(z * e);
                }
            }
        );
    }
}

/// Computes evaluations of the divisor's numerator over the domain of the specified size and offset.
#[allow(clippy::many_single_char_names)]
fn get_inv_evaluation<B: StarkField>(
    divisor: &ConstraintDivisor<B>,
    domain_size: usize,
    domain_offset: B,
) -> Vec<B> {
    let numerator = divisor.numerator();
    let a = numerator[0].0 as u64; // numerator degree
    let b = numerator[0].1;

    let n = domain_size / a as usize;
    let g = B::get_root_of_unity(domain_size.trailing_zeros()).exp(a.into());

    // compute x^a - b for all x
    let mut evaluations = unsafe { uninit_vector(n) };
    batch_iter_mut!(
        &mut evaluations,
        128, // min batch size
        |batch: &mut [B], batch_offset: usize| {
            let mut x = domain_offset.exp(a.into()) * g.exp((batch_offset as u64).into());
            for evaluation in batch.iter_mut() {
                *evaluation = x - b;
                x *= g;
            }
        }
    );

    // compute 1 / (x^a - b)
    batch_inversion(&evaluations)
}

// DEBUG HELPERS
// ================================================================================================

/// Makes sure that the post-division degree of the polynomial matches the expected degree
#[cfg(debug_assertions)]
fn validate_column_degree<B: StarkField, E: FieldElement<BaseField = B>>(
    column: &[E],
    divisor: &ConstraintDivisor<B>,
    domain_offset: B,
    expected_degree: usize,
) -> Result<(), ProverError> {
    // build domain for divisor evaluation, and evaluate it over this domain
    let g = B::get_root_of_unity(column.len().trailing_zeros());
    let domain = math::get_power_series_with_offset(g, domain_offset, column.len());
    let div_values = domain
        .into_iter()
        .map(|x| E::from(divisor.evaluate_at(x)))
        .collect::<Vec<_>>();

    // divide column values by the divisor
    let mut evaluations = column
        .iter()
        .zip(div_values)
        .map(|(&c, d)| c / d)
        .collect::<Vec<_>>();

    // interpolate evaluations into a polynomial in coefficient form
    let inv_twiddles = fft::get_inv_twiddles::<B>(evaluations.len());
    fft::interpolate_poly_with_offset(&mut evaluations, &inv_twiddles, domain_offset);
    let poly = evaluations;

    if expected_degree != math::polynom::degree_of(&poly) {
        return Err(ProverError::MismatchedConstraintPolynomialDegree(
            expected_degree,
            math::polynom::degree_of(&poly),
        ));
    }
    Ok(())
}
