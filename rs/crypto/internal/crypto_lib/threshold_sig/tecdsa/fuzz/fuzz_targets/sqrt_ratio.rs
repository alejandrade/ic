#![no_main]
use libfuzzer_sys::fuzz_target;

use ic_crypto_internal_threshold_sig_ecdsa::*;

/*
hash2curve has two possible implementations of sqrt_ratio, one generic
that works for any curve and another that is optimized for p == 3 (mod 4)

Run both and compare them
*/

fn cmov(
    a: &EccFieldElement,
    b: &EccFieldElement,
    c: subtle::Choice,
) -> ThresholdEcdsaResult<EccFieldElement> {
    let mut r = a.clone();
    r.ct_assign(b, c)?;
    Ok(r)
}

fn sqrt_ratio_generic(
    u: &EccFieldElement,
    v: &EccFieldElement,
) -> ThresholdEcdsaResult<(subtle::Choice, EccFieldElement)> {
    let curve_type = u.curve_type();

    // Generic but slower codepath for other primes
    let z = EccFieldElement::sswu_z(curve_type);
    let vinv = v.invert();
    let uov = u.mul(&vinv)?;
    let (uov_is_qr, sqrt_uov) = uov.sqrt();
    let z_uov = z.mul(&uov)?;
    let (_, sqrt_z_uov) = z_uov.sqrt();
    Ok((uov_is_qr, cmov(&sqrt_z_uov, &sqrt_uov, uov_is_qr)?))
}

fn sqrt_ratio_p_3_mod_4(
    u: &EccFieldElement,
    v: &EccFieldElement,
) -> ThresholdEcdsaResult<(subtle::Choice, EccFieldElement)> {
    let curve_type = u.curve_type();

    // Fast codepath for curves where p == 3 (mod 4)
    // See https://www.ietf.org/archive/id/draft-irtf-cfrg-hash-to-curve-14.html#appendix-F.2.1.2
    let c2 = EccFieldElement::sswu_c2(curve_type);

    let tv1 = v.square()?;
    let tv2 = u.mul(v)?;
    let tv1 = tv1.mul(&tv2)?;
    let y1 = tv1.progenitor(); // see https://eprint.iacr.org/2020/1497.pdf
    let y1 = y1.mul(&tv2)?;
    let y2 = y1.mul(&c2)?;
    let tv3 = y1.square()?;
    let tv3 = tv3.mul(v)?;
    let is_qr = tv3.ct_eq(u)?;
    let y = cmov(&y2, &y1, is_qr)?;
    Ok((is_qr, y))
}

fn sqrt_ratio_fuzz_run(curve_type: EccCurveType, data: &[u8]) {
    let half = data.len() / 2;
    let u = EccFieldElement::from_bytes_wide(curve_type, &data[..half]).unwrap();
    let v = EccFieldElement::from_bytes_wide(curve_type, &data[half..]).unwrap();

    if bool::from(u.is_zero()) {
        // invalid case
        return;
    }

    let refv = sqrt_ratio_generic(&u, &v).unwrap();
    let optv = sqrt_ratio_p_3_mod_4(&u, &v).unwrap();

    assert_eq!(refv.1, optv.1);
}

fuzz_target!(|data: &[u8]| {
    if data.len() != 64 {
        return;
    }
    let _ = sqrt_ratio_fuzz_run(EccCurveType::K256, data);
    let _ = sqrt_ratio_fuzz_run(EccCurveType::P256, data);
});
