use anyhow::{ensure, Result};
use itertools::Itertools;
use plonky2::field::extension_field::Extendable;
use plonky2::field::field_types::Field;
use plonky2::field::polynomial::{PolynomialCoeffs, PolynomialValues};
use plonky2::field::zero_poly_coset::ZeroPolyOnCoset;
use plonky2::fri::oracle::PolynomialBatch;
use plonky2::hash::hash_types::RichField;
use plonky2::iop::challenger::Challenger;
use plonky2::plonk::config::GenericConfig;
use plonky2::timed;
use plonky2::util::timing::TimingTree;
use plonky2::util::transpose;
use plonky2_util::log2_strict;
use rayon::prelude::*;

use crate::config::StarkConfig;
use crate::constraint_consumer::ConstraintConsumer;
use crate::proof::{StarkOpeningSet, StarkProof};
use crate::stark::Stark;
use crate::vars::StarkEvaluationVars;

// TODO: Deal with public inputs.
pub fn prove<F, C, S, const D: usize>(
    stark: S,
    config: StarkConfig,
    trace: Vec<[F; S::COLUMNS]>,
    public_inputs: [F; S::PUBLIC_INPUTS],
    timing: &mut TimingTree,
) -> Result<StarkProof<F, C, D>>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    S: Stark<F, D>,
    [(); S::COLUMNS]:,
    [(); S::PUBLIC_INPUTS]:,
{
    let degree = trace.len();
    let degree_bits = log2_strict(degree);

    let trace_vecs = trace.into_iter().map(|row| row.to_vec()).collect_vec();
    let trace_col_major: Vec<Vec<F>> = transpose(&trace_vecs);

    let trace_poly_values: Vec<PolynomialValues<F>> = timed!(
        timing,
        "compute trace polynomials",
        trace_col_major
            .par_iter()
            .map(|column| PolynomialValues::new(column.clone()))
            .collect()
    );

    let rate_bits = config.fri_config.rate_bits;
    let cap_height = config.fri_config.cap_height;
    let trace_commitment = timed!(
        timing,
        "compute trace commitment",
        PolynomialBatch::<F, C, D>::from_values(
            trace_poly_values,
            rate_bits,
            false,
            cap_height,
            timing,
            None,
        )
    );

    let trace_cap = trace_commitment.merkle_tree.cap.clone();
    let mut challenger = Challenger::new();
    challenger.observe_cap(&trace_cap);

    let alphas = challenger.get_n_challenges(config.num_challenges);
    let quotient_polys = compute_quotient_polys::<F, C, S, D>(
        &stark,
        &trace_commitment,
        public_inputs,
        alphas,
        degree_bits,
        rate_bits,
    );
    let all_quotient_chunks = quotient_polys
        .into_par_iter()
        .flat_map(|mut quotient_poly| {
            quotient_poly.trim();
            quotient_poly
                .pad(degree << rate_bits)
                .expect("Quotient has failed, the vanishing polynomial is not divisible by `Z_H");
            // Split quotient into degree-n chunks.
            quotient_poly.chunks(degree)
        })
        .collect();
    let quotient_commitment = timed!(
        timing,
        "compute quotient commitment",
        PolynomialBatch::from_coeffs(
            all_quotient_chunks,
            rate_bits,
            false,
            config.fri_config.cap_height,
            timing,
            None,
        )
    );
    challenger.observe_cap(&quotient_commitment.merkle_tree.cap);

    let zeta = challenger.get_extension_challenge::<D>();
    // To avoid leaking witness data, we want to ensure that our opening locations, `zeta` and
    // `g * zeta`, are not in our subgroup `H`. It suffices to check `zeta` only, since
    // `(g * zeta)^n = zeta^n`, where `n` is the order of `g`.
    let g = F::Extension::primitive_root_of_unity(degree_bits);
    ensure!(
        zeta.exp_power_of_2(degree_bits) != F::Extension::ONE,
        "Opening point is in the subgroup."
    );
    let openings = StarkOpeningSet::new(zeta, g, &trace_commitment, &quotient_commitment);

    // TODO: Add permuation checks
    let initial_merkle_trees = &[&trace_commitment, &quotient_commitment];
    let fri_params = config.fri_params(degree_bits);

    let opening_proof = timed!(
        timing,
        "compute openings proof",
        PolynomialBatch::prove_openings(
            &S::fri_instance(zeta, g, rate_bits),
            initial_merkle_trees,
            &mut challenger,
            &fri_params,
            timing,
        )
    );

    Ok(StarkProof {
        trace_cap,
        openings,
        opening_proof,
    })
}

/// Computes the quotient polynomials `(sum alpha^i C_i(x)) / Z_H(x)` for `alpha` in `alphas`,
/// where the `C_i`s are the Stark constraints.
// TODO: This won't work for the Fibonacci example because the constraints wrap around the subgroup.
// The denominator should be the vanishing polynomial of `H` without its last element.
fn compute_quotient_polys<F, C, S, const D: usize>(
    stark: &S,
    trace_commitment: &PolynomialBatch<F, C, D>,
    public_inputs: [F; S::PUBLIC_INPUTS],
    alphas: Vec<F>,
    degree_bits: usize,
    rate_bits: usize,
) -> Vec<PolynomialCoeffs<F>>
where
    F: RichField + Extendable<D>,
    C: GenericConfig<D, F = F>,
    S: Stark<F, D>,
    [(); S::COLUMNS]:,
    [(); S::PUBLIC_INPUTS]:,
{
    let degree = 1 << degree_bits;
    let points = F::two_adic_subgroup(degree_bits + rate_bits);

    // Evaluation of the first Lagrange polynomial on the LDE domain.
    let lagrange_first = {
        let mut evals = PolynomialValues::new(vec![F::ZERO; degree]);
        evals.values[0] = F::ONE;
        evals.lde(rate_bits)
    };
    // Evaluation of the last Lagrange polynomial on the LDE domain.
    let lagrange_last = {
        let mut evals = PolynomialValues::new(vec![F::ZERO; degree]);
        evals.values[degree - 1] = F::ONE;
        evals.lde(rate_bits)
    };

    let z_h_on_coset = ZeroPolyOnCoset::new(degree_bits, rate_bits);

    // Retrieve the LDE values at index `i`.
    let get_at_index = |comm: &PolynomialBatch<F, C, D>, i: usize| -> [F; S::COLUMNS] {
        comm.get_lde_values(i).try_into().unwrap()
    };

    let quotient_values = (0..degree << rate_bits)
        .into_par_iter()
        .map(|i| {
            // TODO: Set `P` to a genuine `PackedField` here.
            let mut consumer = ConstraintConsumer::<F>::new(
                alphas.clone(),
                lagrange_first.values[i],
                lagrange_last.values[i],
            );
            let vars = StarkEvaluationVars::<F, F, { S::COLUMNS }, { S::PUBLIC_INPUTS }> {
                local_values: &get_at_index(trace_commitment, i),
                next_values: &get_at_index(trace_commitment, (i + 1) % (degree << rate_bits)),
                public_inputs: &public_inputs,
            };
            stark.eval_packed_base(vars, &mut consumer);
            // TODO: Fix this once we a genuine `PackedField`.
            let mut constraints_evals = consumer.accumulators();
            let denominator_inv = z_h_on_coset.eval_inverse(i);
            for eval in &mut constraints_evals {
                *eval *= denominator_inv;
            }
            constraints_evals
        })
        .collect::<Vec<_>>();

    transpose(&quotient_values)
        .into_par_iter()
        .map(PolynomialValues::new)
        .map(|values| values.coset_ifft(F::coset_shift()))
        .collect()
}