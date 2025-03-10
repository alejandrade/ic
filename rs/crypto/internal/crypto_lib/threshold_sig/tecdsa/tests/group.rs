use ic_crypto_internal_threshold_sig_ecdsa::*;
use ic_crypto_test_utils_reproducible_rng::reproducible_rng;

#[test]
fn not_affected_by_point_serialization_bug() -> ThresholdEcdsaResult<()> {
    // Repro of https://github.com/RustCrypto/elliptic-curves/issues/529
    let curve = EccCurveType::K256;

    let pts = [
        "024b395881d9965c4621459ad2ec12716fa7f669b6108ad3b8b82b91644fb44808",
        "02e77d7b458fb3a2df7d201806e8e1dbce8c1138303156c43398ac62891c43e3cc",
        "02f973e12be0ea160cc82c16563753749b5e6590d22a0b9ab16cd48b9bd951b167",
    ];

    for pt in pts {
        let bytes = hex::decode(pt).unwrap();
        let pt = EccPoint::deserialize(curve, &bytes)?;
        let pt_bytes = pt.serialize();

        assert_eq!(bytes, pt_bytes);
    }

    Ok(())
}

#[test]
fn verify_serialization_round_trips_correctly() -> ThresholdEcdsaResult<()> {
    fn assert_serialization_round_trips(pt: EccPoint) {
        let curve_type = pt.curve_type();
        let b = pt.serialize();

        assert_eq!(b.len(), curve_type.point_bytes());

        let pt2 = EccPoint::deserialize(curve_type, &b)
            .expect("Failed to deserialize the point serialization");

        assert_eq!(pt, pt2);

        let b2 = pt2.serialize();
        assert_eq!(b, b2);
    }

    let mut rng = reproducible_rng();

    for curve_type in EccCurveType::all() {
        let identity = EccPoint::identity(curve_type);

        // Identity should consist entirely of zero bytes
        assert!(identity.serialize().iter().all(|x| *x == 0x00));

        assert_serialization_round_trips(identity);
        assert_serialization_round_trips(EccPoint::generator_g(curve_type)?);
        assert_serialization_round_trips(EccPoint::generator_h(curve_type)?);

        for _r in 0..100 {
            let s = EccScalar::random(curve_type, &mut rng);
            let gs = EccPoint::mul_by_g(&s)?;

            assert_serialization_round_trips(gs);
        }
    }

    Ok(())
}

#[test]
fn hash_to_scalar_is_deterministic() -> ThresholdEcdsaResult<()> {
    let input = "test input string".as_bytes();
    let domain_separator = "domain sep".as_bytes();

    for curve_type in EccCurveType::all() {
        let s1 = EccScalar::hash_to_scalar(curve_type, input, domain_separator)?;
        let s2 = EccScalar::hash_to_scalar(curve_type, input, domain_separator)?;

        assert_eq!(hex::encode(s1.serialize()), hex::encode(s2.serialize()));
    }

    Ok(())
}

#[test]
fn hash_to_scalar_p256_has_fixed_output() -> ThresholdEcdsaResult<()> {
    let curve_type = EccCurveType::P256;
    let input = "known answer test input".as_bytes();
    let domain_separator = "domain sep".as_bytes();

    let s = EccScalar::hash_to_scalar(curve_type, input, domain_separator)?;

    assert_eq!(
        hex::encode(s.serialize()),
        "8bfdfb742b025311d5e80a5070e2d074ea03c363d16cd3548debc3a408729d37"
    );

    Ok(())
}

#[test]
fn hash_to_scalar_k256_has_fixed_output() -> ThresholdEcdsaResult<()> {
    let curve_type = EccCurveType::K256;
    let input = "known answer test input".as_bytes();
    let domain_separator = "domain sep".as_bytes();

    let s = EccScalar::hash_to_scalar(curve_type, input, domain_separator)?;

    assert_eq!(
        hex::encode(s.serialize()),
        "3670f931a6cbff777594bf1488812b63895dfe5df9814584dfd231f69a66541a"
    );

    Ok(())
}

#[test]
fn generator_h_has_expected_value() -> ThresholdEcdsaResult<()> {
    for curve_type in EccCurveType::all() {
        let h = EccPoint::generator_h(curve_type)?;

        let input = "h";
        let dst = format!("ic-crypto-tecdsa-{}-generator-h", curve_type);

        let h2p = EccPoint::hash_to_point(curve_type, input.as_bytes(), dst.as_bytes())?;

        assert_eq!(h, h2p);
    }
    Ok(())
}

#[test]
fn k256_wide_reduce_scalar_expected_value() -> ThresholdEcdsaResult<()> {
    // Checked using Python
    let wide_input = hex::decode("5465872a72824a73539f16e825035c403a2596407116900d47141fca8cbfd9a638af75a71310b08fe6351dd302b820c86b15e71ea73c78c876c1f88338a0").unwrap();

    let scalar = EccScalar::from_bytes_wide(EccCurveType::K256, &wide_input)?;

    assert_eq!(
        hex::encode(scalar.serialize()),
        "5bc912d1f858a44805b5bcf9809751eb7ca8cd5efe9b9bef62374b55a857ba1b"
    );

    Ok(())
}

#[test]
fn p256_wide_reduce_scalar_expected_value() -> ThresholdEcdsaResult<()> {
    // Checked using Python
    let wide_input = hex::decode("5465872a72824a73539f16e825035c403a2596407116900d47141fca8cbfd9a638af75a71310b08fe6351dd302b820c86b15e71ea73c78c876c1f88338a0").unwrap();

    let scalar = EccScalar::from_bytes_wide(EccCurveType::P256, &wide_input)?;

    assert_eq!(
        hex::encode(scalar.serialize()),
        "87b5343f875ced075916b4d84e1642aebe8784bd914295c51e484d133595b57e"
    );

    Ok(())
}

#[test]
fn test_scalar_negate() -> ThresholdEcdsaResult<()> {
    let mut rng = reproducible_rng();

    for curve in EccCurveType::all() {
        let zero = EccScalar::zero(curve);

        for _trial in 0..100 {
            let random = EccScalar::random(curve, &mut rng);
            let n_random = random.negate();
            let should_be_zero = random.add(&n_random)?;
            assert_eq!(should_be_zero, zero);
            assert!(should_be_zero.is_zero());

            let should_be_zero = n_random.add(&random)?;
            assert_eq!(should_be_zero, zero);
            assert!(should_be_zero.is_zero());
        }
    }

    Ok(())
}

#[test]
fn test_point_mul_by_node_index() -> ThresholdEcdsaResult<()> {
    for curve in EccCurveType::all() {
        let g = EccPoint::generator_g(curve)?;

        for node_index in 0..300 {
            let g_ni = g.mul_by_node_index(node_index)?;

            let scalar = EccScalar::from_node_index(curve, node_index);
            let g_s = g.scalar_mul(&scalar)?;

            assert_eq!(g_s, g_ni);
        }
    }

    Ok(())
}

#[test]
fn test_point_mul_naf() -> ThresholdEcdsaResult<()> {
    let mut rng = reproducible_rng();
    for curve_type in EccCurveType::all() {
        for window_size in [3, 4, 5, 6, 7] {
            // 0, 1, -1 (maximum value), 100 random values
            let mut scalars = Vec::with_capacity(103);
            scalars.push(EccScalar::zero(curve_type));
            scalars.push(EccScalar::one(curve_type));
            scalars.push(EccScalar::one(curve_type).negate());
            for _ in 0..100 {
                scalars.push(EccScalar::random(curve_type, &mut rng));
            }

            // test correctness for the generated scalars
            for scalar in scalars {
                // random point
                let random_scalar = EccScalar::random(curve_type, &mut rng);
                let mut random_point = EccPoint::mul_by_g(&random_scalar)?;
                let expected_point = random_point.scalar_mul(&scalar)?;
                assert!(!random_point.is_precopmuted());
                random_point.precompute(window_size)?;
                assert!(random_point.is_precopmuted());
                let computed_point = random_point.scalar_mul_vartime(&scalar)?;
                assert_eq!(computed_point, expected_point);
            }
        }
    }

    Ok(())
}

#[test]
fn test_point_negate() -> ThresholdEcdsaResult<()> {
    let mut rng = reproducible_rng();

    for curve_type in EccCurveType::all() {
        let id = EccPoint::identity(curve_type);
        let g = EccPoint::generator_g(curve_type)?;

        assert_eq!(id.negate(), id);

        for _trial in 0..100 {
            let random_scalar = EccScalar::random(curve_type, &mut rng);
            let random_point = g.scalar_mul(&random_scalar)?;
            let n_random_point = random_point.negate();

            let should_be_zero = random_point.add_points(&n_random_point)?;
            assert_eq!(should_be_zero, id);

            let should_be_zero = n_random_point.add_points(&random_point)?;
            assert_eq!(should_be_zero, id);
        }
    }
    Ok(())
}

#[test]
fn test_mul_2_is_correct() -> ThresholdEcdsaResult<()> {
    let mut rng = reproducible_rng();

    for curve_type in EccCurveType::all() {
        let g = EccPoint::generator_g(curve_type)?;

        for _iteration in 0..100 {
            let p_0 = g.scalar_mul(&EccScalar::random(curve_type, &mut rng))?;
            let p_1 = g.scalar_mul(&EccScalar::random(curve_type, &mut rng))?;

            let s_0 = EccScalar::random(curve_type, &mut rng);
            let s_1 = EccScalar::random(curve_type, &mut rng);

            let computed_result = EccPoint::mul_2_points(&p_0, &s_0, &p_1, &s_1)?;
            let expected_result = p_0.scalar_mul(&s_0)?.add_points(&p_1.scalar_mul(&s_1)?)?;
            assert_eq!(computed_result, expected_result);
        }
    }

    Ok(())
}

#[test]
fn test_mul_n_ct_pippenger_is_correct() -> ThresholdEcdsaResult<()> {
    let mut rng = reproducible_rng();
    let mut random_point_and_scalar = |curve_type| -> ThresholdEcdsaResult<(EccPoint, EccScalar)> {
        let p = EccPoint::mul_by_g(&EccScalar::random(curve_type, &mut rng))?;
        Ok((p, EccScalar::random(curve_type, &mut rng)))
    };

    for curve_type in EccCurveType::all() {
        for num_terms in 2..20 {
            // generate point-scalar pairs
            let pairs: Vec<_> = (0..num_terms)
                .map(|_| (random_point_and_scalar(curve_type)))
                .collect::<Result<Vec<_>, _>>()?;

            // create "deep" refs of pairs
            let refs_of_pairs: Vec<(&EccPoint, &EccScalar)> =
                pairs.iter().map(|pair| (&pair.0, &pair.1)).collect();

            // compute the result using an optimized algorithm, which is to be tested
            let computed_result = EccPoint::mul_n_points_pippenger(&refs_of_pairs)?;

            let mul_and_aggregate =
                |acc: &EccPoint, p: EccPoint, s: EccScalar| -> ThresholdEcdsaResult<EccPoint> {
                    let mul = p.scalar_mul(&s)?;
                    acc.add_points(&mul)
                };
            // compute the result using a non-optimized algorithm, which is assumed to always be correct
            let expected_result =
                pairs
                    .into_iter()
                    .try_fold(EccPoint::identity(curve_type), |acc, pair| {
                        let (p, s) = pair;
                        // acc += p * s
                        mul_and_aggregate(&acc, p, s)
                    })?;
            assert_eq!(computed_result, expected_result);
        }
    }
    Ok(())
}

#[test]
fn test_mul_n_vartime_naf() -> ThresholdEcdsaResult<()> {
    assert_eq!(EccPoint::MIN_LUT_WINDOW_SIZE, 3);
    assert_eq!(EccPoint::MAX_LUT_WINDOW_SIZE, 7);

    let mut rng = reproducible_rng();

    for curve_type in EccCurveType::all() {
        for window_size in [3, 4, 5, 6, 7] {
            let g = EccPoint::generator_g(curve_type)?;
            let mut random_pair = || -> ThresholdEcdsaResult<_> {
                Ok((
                    g.scalar_mul(&EccScalar::random(curve_type, &mut rng))?,
                    EccScalar::random(curve_type, &mut rng),
                ))
            };

            for num_terms in [1, 2, 3, 4, 5, 10, 30, 50] {
                // generate point-scalar pairs
                let mut pairs: Vec<_> = (0..num_terms)
                    .map(|_| random_pair())
                    .collect::<Result<Vec<_>, _>>()?;

                // compute the result using a non-optimized algorithm, which is assumed to always be correct
                let expected_result =
                    pairs
                        .iter()
                        .try_fold(EccPoint::identity(curve_type), |acc, pair| {
                            let (p, s) = &pair;
                            // acc += p * s
                            acc.add_points(&p.scalar_mul(s)?)
                        })?;

                for (p, _s) in pairs.iter_mut() {
                    assert!(!p.is_precopmuted());
                    p.precompute(window_size)?;
                    assert!(p.is_precopmuted());
                }

                // create refs of pairs
                let refs_of_pairs: Vec<_> = pairs.iter().map(|(p, s)| (p, s)).collect();

                // compute the result using an optimized algorithm, which is to be tested
                let computed_result = EccPoint::mul_n_points_vartime(&refs_of_pairs[..])?;

                assert_eq!(computed_result, expected_result);
            }
        }
    }

    Ok(())
}
