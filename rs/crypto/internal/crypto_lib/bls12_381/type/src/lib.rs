//! Wrapper for BLS12-381 operations

#![forbid(unsafe_code)]
#![forbid(missing_docs)]
#![warn(rust_2018_idioms)]
#![warn(future_incompatible)]
#![allow(clippy::needless_range_loop)]

#[cfg(test)]
mod tests;

use ic_bls12_381::hash_to_curve::{ExpandMsgXmd, HashToCurve};
use pairing::group::{ff::Field, Group};
use paste::paste;
use rand::{CryptoRng, RngCore};
use std::fmt;
use std::sync::Arc;
use zeroize::{Zeroize, ZeroizeOnDrop};

macro_rules! ctoption_ok_or {
    ($val:expr, $err:expr) => {
        if bool::from($val.is_some()) {
            Ok(Self::new($val.unwrap()))
        } else {
            Err($err)
        }
    };
}

/// Error returned if a point encoding is invalid
#[derive(Copy, Clone, Debug)]
pub enum PairingInvalidPoint {
    /// The point encoding was invalid
    InvalidPoint,
}

/// Error returned if a scalar encoding is invalid
#[derive(Copy, Clone, Debug)]
pub enum PairingInvalidScalar {
    /// The scalar encoding was invalid
    InvalidScalar,
}

/// An integer of the order of the groups G1/G2/Gt
#[derive(Clone, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub struct Scalar {
    value: ic_bls12_381::Scalar,
}

impl Ord for Scalar {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // We assume ct_compare returns < 0 for less than, == 0 for equals
        // and > 0 for greater than. This is a looser contract than what
        // ct_compare actually does but it avoids having to include a
        // panic or unreachable! invocation.
        self.ct_compare(other).cmp(&0)
    }
}

impl PartialOrd for Scalar {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

macro_rules! impl_debug_using_serialize_for {
    ( $typ:ty ) => {
        impl fmt::Debug for $typ {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($typ), hex::encode(self.serialize()))
            }
        }
    };
}

impl_debug_using_serialize_for!(Scalar);

impl Scalar {
    /// The size in bytes of this type
    pub const BYTES: usize = 32;

    /// Create a new Scalar from the inner type
    pub(crate) fn new(value: ic_bls12_381::Scalar) -> Self {
        Self { value }
    }

    /// Return the inner value
    pub(crate) fn inner(&self) -> &ic_bls12_381::Scalar {
        &self.value
    }

    /// Create a scalar from a small integer value
    pub fn from_u64(v: u64) -> Self {
        let value: [u64; 4] = [v, 0, 0, 0];
        Self::new(ic_bls12_381::Scalar::from_raw(value))
    }

    /// Create a scalar from a small integer value
    pub fn from_u32(v: u32) -> Self {
        Self::from_u64(v as u64)
    }

    /// Create a scalar from a small integer value
    pub fn from_i32(v: i32) -> Self {
        if v < 0 {
            Self::from_u64((v as i64).abs() as u64).neg()
        } else {
            Self::from_u64(v.abs() as u64)
        }
    }

    /// Create a scalar from a small integer value
    pub fn from_usize(v: usize) -> Self {
        Self::from_u64(v as u64)
    }

    /// Create a scalar from a small integer value
    pub fn from_isize(v: isize) -> Self {
        if v < 0 {
            Self::from_u64((v as i64).abs() as u64).neg()
        } else {
            Self::from_u64(v.abs() as u64)
        }
    }

    /// Return `cnt` consecutive powers of `x`
    pub fn xpowers(x: &Self, cnt: usize) -> Vec<Self> {
        let mut r = Vec::with_capacity(cnt);

        let mut xpow = Self::one();
        for _ in 0..cnt {
            xpow *= x;
            r.push(xpow.clone());
        }

        r
    }

    /// Randomly generate a scalar in a way that is compatible with MIRACL
    ///
    /// This should not be used for new code but only for compatability in
    /// situations where MIRACL's BIG::randomnum was previously used
    pub fn miracl_random<R: RngCore + CryptoRng>(rng: &mut R) -> Self {
        /*
        MIRACL's BIG::randomnum implementation uses an unusually inefficient
        approach to generating a random integer in a prime field. Effectively it
        generates a random bitstring of length twice the length of that of the
        prime (here, the 255-bit BLS12-381 prime subgroup order), and executes a
        double-and-add algorithm, one bit at a time. As a result, the final bit
        that was generated is equal to the *lowest order bit* in the result.
        Finally, it performs a modular reduction on the generated 510 bit
        integer.

        To replicate this behavior we have to reverse the bits within each byte,
        and then reverse the bytes as well. This creates `val` which is equal
        to MIRACL's result after 504 iterations of the loop in randomnum.

        The final 6 bits are handled by using 6 doublings to shift the Scalar value
        up to provide space, followed by a scalar addition.
        */

        use rand::Rng;

        let mut bytes = [0u8; 64];

        // We can't use fill_bytes here because that results in incompatible output.
        for i in 0..64 {
            bytes[i] = rng.gen::<u8>();
        }

        let mut rbuf = [0u8; 64];
        for j in 0..63 {
            rbuf[j] = bytes[62 - j].reverse_bits();
        }

        let mut val = Self::new(ic_bls12_381::Scalar::from_bytes_wide(&rbuf));

        for _ in 0..6 {
            val = val.double();
        }
        val += Scalar::from_u32((bytes[63].reverse_bits() >> 2) as u32);

        val
    }

    /// Return the scalar 0
    pub fn zero() -> Self {
        Self::new(ic_bls12_381::Scalar::zero())
    }

    /// Return the scalar 1
    pub fn one() -> Self {
        Self::new(ic_bls12_381::Scalar::one())
    }

    /// Return true iff this value is zero
    pub fn is_zero(&self) -> bool {
        bool::from(self.value.is_zero())
    }

    /// Return the additive inverse of this scalar
    pub fn neg(&self) -> Self {
        Self::new(self.value.neg())
    }

    /// Double this scalar
    pub fn double(&self) -> Self {
        Self::new(self.value.double())
    }

    /// Return the multiplicative inverse of this scalar if it exists
    pub fn inverse(&self) -> Option<Self> {
        let inv = self.value.invert();
        if bool::from(inv.is_some()) {
            Some(Self::new(inv.unwrap()))
        } else {
            None
        }
    }

    /// Return a random scalar
    pub fn random<R: RngCore + CryptoRng>(rng: &mut R) -> Self {
        loop {
            /*
            A BLS12-381 scalar is 255 bits long. Generate the scalar using
            rejection sampling by creating a 255 bit random bitstring then
            checking if it is less than the group order.
            */
            let mut buf = [0u8; Self::BYTES];
            rng.fill_bytes(&mut buf);
            buf[0] &= 0b0111_1111; // clear the 256th bit

            if let Ok(s) = Self::deserialize(&buf) {
                return s;
            }
        }
    }

    /// Return several random scalars
    pub fn batch_random<R: RngCore + CryptoRng>(rng: &mut R, count: usize) -> Vec<Self> {
        let mut result = Vec::with_capacity(count);

        for _ in 0..count {
            result.push(Self::random(rng));
        }

        result
    }

    /// Return a random scalar within a small range
    ///
    /// Returns a scalar in range [0,n) using rejection sampling.
    pub fn random_within_range<R: RngCore + CryptoRng>(rng: &mut R, n: u64) -> Self {
        if n <= 1 {
            return Self::zero();
        }

        let t_bits = std::mem::size_of::<u64>() * 8;
        let n_bits = std::cmp::min(255, t_bits - n.leading_zeros() as usize);
        let n_bytes = (n_bits + 7) / 8;
        let n_mask = if n_bits % 8 == 0 {
            0xFF
        } else {
            0xFF >> (8 - n_bits % 8)
        };

        let n = Scalar::from_u64(n);

        loop {
            let mut buf = [0u8; Self::BYTES];
            rng.fill_bytes(&mut buf[Self::BYTES - n_bytes..]);
            buf[Self::BYTES - n_bytes] &= n_mask;

            if let Ok(s) = Self::deserialize(&buf) {
                if s < n {
                    return s;
                }
            }
        }
    }

    /// Decode a scalar as a big-endian byte string, accepting out of range elements
    ///
    /// Out of range elements are reduced modulo the group order
    pub fn deserialize_unchecked(bytes: [u8; Self::BYTES]) -> Self {
        let mut le_bytes = [0u8; 64];

        for i in 0..Self::BYTES {
            le_bytes[i] = bytes[Self::BYTES - i - 1];
        }
        // le_bytes[32..64] left as zero
        Self::new(ic_bls12_381::Scalar::from_bytes_wide(&le_bytes))
    }

    /// Deserialize a scalar from a big-endian byte string
    pub fn deserialize<B: AsRef<[u8]>>(bytes: &B) -> Result<Self, PairingInvalidScalar> {
        let mut bytes: [u8; Self::BYTES] = bytes
            .as_ref()
            .try_into()
            .map_err(|_| PairingInvalidScalar::InvalidScalar)?;
        bytes.reverse();
        let scalar = ic_bls12_381::Scalar::from_bytes(&bytes);
        ctoption_ok_or!(scalar, PairingInvalidScalar::InvalidScalar)
    }

    /// Deserialize multiple scalars
    ///
    /// This function returns Ok only if all of the provided inputs
    /// represent a valid scalar.
    pub fn batch_deserialize<B: AsRef<[u8]>>(
        inputs: &[B],
    ) -> Result<Vec<Self>, PairingInvalidScalar> {
        let mut r = Vec::with_capacity(inputs.len());
        for input in inputs {
            r.push(Self::deserialize(input)?);
        }
        Ok(r)
    }

    /// Serialize the scalar to a big-endian byte string
    pub fn serialize(&self) -> [u8; Self::BYTES] {
        let mut bytes = self.value.to_bytes();
        bytes.reverse();
        bytes
    }

    /// Serialize the scalar to a big-endian byte string in some specific type
    pub fn serialize_to<T: From<[u8; Self::BYTES]>>(&self) -> T {
        T::from(self.serialize())
    }

    /// Multiscalar multiplication
    ///
    /// Equivalent to p1*s1 + p2*s2 + p3*s3 + ... + pn*sn
    ///
    /// Returns zero if terms is empty
    ///
    /// Warning: this function may leak information about the scalars via
    /// memory-based side channels. Do not use this function with secret
    /// scalars. For the purposes of this warning, the first element of
    /// the tuple may be a secret, while the values of the second element
    /// of the tuple could leak to an attacker.
    ///
    /// Warning: if lhs.len() != rhs.len() this function ignores trailing elements
    /// of the longer slice.
    ///
    /// Currently only a naive version is implemented.
    pub fn muln_vartime(lhs: &[Self], rhs: &[Self]) -> Self {
        let terms = std::cmp::min(lhs.len(), rhs.len());
        let mut accum = Self::zero();
        for i in 0..terms {
            accum += &lhs[i] * &rhs[i];
        }
        accum
    }

    /// Multiscalar multiplication with usize multiplicands
    ///
    /// Equivalent to p1*s1 + p2*s2 + p3*s3 + ... + pn*sn
    ///
    /// Returns zero if terms is empty
    ///
    /// Warning: this function may leak information about the usize values via
    /// memory-based side channels. Do not use this function with secret usize
    /// arguments.
    ///
    /// Warning: if lhs.len() != rhs.len() this function ignores trailing elements
    /// of the longer slice.
    ///
    /// Currently only a naive version is implemented.
    ///
    /// This function could take advantage of the fact that rhs is known to be
    /// at most 64 bits, limiting the number of doublings.
    pub fn muln_usize_vartime(lhs: &[Self], rhs: &[usize]) -> Self {
        let terms = std::cmp::min(lhs.len(), rhs.len());
        let mut accum = Self::zero();
        for i in 0..terms {
            accum += &lhs[i] * Scalar::from_usize(rhs[i]);
        }
        accum
    }

    /// Compare a Scalar with another
    ///
    /// If self < other returns -1
    /// If self == other returns 0
    /// If self > other returns 1
    pub(crate) fn ct_compare(&self, other: &Self) -> i8 {
        use subtle::{ConditionallySelectable, ConstantTimeEq, ConstantTimeLess};

        const IS_LT: u8 = 0xff; // -1i8 as u8
        const IS_EQ: u8 = 0;
        const IS_GT: u8 = 1;

        let a = self.serialize();
        let b = other.serialize();

        /*
        ic_bls12_381::Scalar does not implement comparisons natively.

        Perform this operation by comparing the serializations of the Scalar
        instead.

        This function is equivalent to self.serialize().cmp(other.serialize())
        except that it runs in constant time to avoid leaking information about
        the values.

        The logic works by examining each byte, starting from the least
        significant (in a[Self::BYTES-1]) and working up to the most significant
        (in a[0]).  At each step we track (in variable `result`) what the
        comparison would have resulted in had we just compared up to that point
        (ignoring the higher order bytes)

        If the two bytes we are comparing are the same, then whatever their
        value is does not change the result. As an example, XY and XZ have the
        same comparison result as Y and Z would, for any three bytes X, Y, Z.

        If they are not the same then either x is less than y, or it is not
        (which implies, since we know x != y, that x is strictly greater than
        y).  Additionally, since the byte we are examining at this point has
        greater magnitude than any byte we have looked at previously, the result
        we have computed so far no longer matters.

        Pseudo-code for this loop would be:

        let mut result = IS_EQ;
        for (x,y) in (&a, &b) {
           if x == y { continue; }
           else if x < y { result = IS_LT; }
           else { result = IS_GT; }
        }
        */

        // Return a if c otherwise b
        fn ct_select(c: subtle::Choice, a: u8, b: u8) -> u8 {
            let mut r = b;
            r.conditional_assign(&a, c);
            r
        }

        let mut result = IS_EQ;

        for i in (0..Self::BYTES).rev() {
            let is_lt = u8::ct_lt(&a[i], &b[i]);
            let is_eq = u8::ct_eq(&a[i], &b[i]);

            result = ct_select(is_eq, result, ct_select(is_lt, IS_LT, IS_GT));
        }

        result as i8
    }
}

macro_rules! declare_addsub_ops_for {
    ( $typ:ty ) => {
        impl std::ops::Add<&$typ> for &$typ {
            type Output = $typ;

            fn add(self, other: &$typ) -> $typ {
                <$typ>::new(self.inner() + other.inner())
            }
        }

        impl std::ops::Add<$typ> for $typ {
            type Output = $typ;

            fn add(self, other: $typ) -> $typ {
                &self + &other
            }
        }

        impl std::ops::Add<&$typ> for $typ {
            type Output = $typ;

            fn add(self, other: &$typ) -> $typ {
                &self + other
            }
        }

        impl std::ops::Sub<&$typ> for &$typ {
            type Output = $typ;

            fn sub(self, other: &$typ) -> $typ {
                <$typ>::new(self.inner() - other.inner())
            }
        }

        impl std::ops::Sub<$typ> for $typ {
            type Output = $typ;

            fn sub(self, other: $typ) -> $typ {
                &self - &other
            }
        }

        impl std::ops::Sub<&$typ> for $typ {
            type Output = $typ;

            fn sub(self, other: &$typ) -> $typ {
                &self - other
            }
        }

        impl std::ops::AddAssign for $typ {
            fn add_assign(&mut self, other: Self) {
                self.value += other.inner()
            }
        }

        impl std::ops::AddAssign<&$typ> for $typ {
            fn add_assign(&mut self, other: &Self) {
                self.value += other.inner()
            }
        }

        impl std::ops::SubAssign for $typ {
            fn sub_assign(&mut self, other: Self) {
                self.value -= other.inner()
            }
        }

        impl std::ops::SubAssign<&$typ> for $typ {
            fn sub_assign(&mut self, other: &Self) {
                self.value -= other.inner()
            }
        }
    };
}

macro_rules! declare_mixed_addition_ops_for {
    ( $proj:ty, $affine:ty ) => {
        impl std::ops::Add<&$affine> for &$proj {
            type Output = $proj;

            fn add(self, other: &$affine) -> $proj {
                <$proj>::new(self.inner().add_mixed(other.inner()))
            }
        }

        impl std::ops::Add<$affine> for $proj {
            type Output = $proj;

            fn add(self, other: $affine) -> $proj {
                &self + &other
            }
        }

        impl std::ops::Add<&$affine> for $proj {
            type Output = $proj;

            fn add(self, other: &$affine) -> $proj {
                &self + other
            }
        }

        impl std::ops::AddAssign<$affine> for $proj {
            fn add_assign(&mut self, other: $affine) {
                self.value = self.inner().add_mixed(other.inner());
            }
        }

        impl std::ops::AddAssign<&$affine> for $proj {
            fn add_assign(&mut self, other: &$affine) {
                self.value = self.inner().add_mixed(other.inner());
            }
        }
    };
}

macro_rules! declare_mul_scalar_ops_for {
    ( $typ:ty ) => {
        impl std::ops::Mul<&Scalar> for &$typ {
            type Output = $typ;
            fn mul(self, scalar: &Scalar) -> $typ {
                <$typ>::new(self.inner() * scalar.inner())
            }
        }

        impl std::ops::Mul<&Scalar> for $typ {
            type Output = $typ;
            fn mul(self, scalar: &Scalar) -> $typ {
                &self * scalar
            }
        }

        impl std::ops::Mul<Scalar> for &$typ {
            type Output = $typ;
            fn mul(self, scalar: Scalar) -> $typ {
                self * &scalar
            }
        }

        impl std::ops::Mul<Scalar> for $typ {
            type Output = $typ;
            fn mul(self, scalar: Scalar) -> $typ {
                &self * &scalar
            }
        }

        impl std::ops::MulAssign<Scalar> for $typ {
            fn mul_assign(&mut self, other: Scalar) {
                self.value *= other.inner()
            }
        }

        impl std::ops::MulAssign<&Scalar> for $typ {
            fn mul_assign(&mut self, other: &Scalar) {
                self.value *= other.inner()
            }
        }
    };
}

declare_addsub_ops_for!(Scalar);
declare_mul_scalar_ops_for!(Scalar);

macro_rules! define_affine_and_projective_types {
    ( $affine:ident, $projective:ident, $size:expr ) => {
        paste! {
            lazy_static::lazy_static! {
                static ref [<$affine:upper _GENERATOR>] : $affine = $affine::new_with_precomputation(ic_bls12_381::$affine::generator());
            }
        }

        paste! {
            #[derive(Zeroize, ZeroizeOnDrop)]
            /// Structure for fast multiplication for known/fixed points
            ///
            /// This algorithm works by precomputing a table such that by adding
            /// together selected elements of the table, a scalar multiplication is
            /// effected without any doublings.
            ///
            /// Each window of the scalar has its own set of elements in the table,
            /// which are not used for any other window. An implicit element of each
            /// set is the identity element, which is omitted to save space in the
            /// table. (However this does make some of the indexing operations less
            /// obvious.)
            ///
            /// The simplest version to understand is the 1-bit window case.  There, we
            /// compute a table containing P,P*2^1,...,P*2^255, and for each bit of the
            /// scalar conditionally add that power of P.  To make this constant time
            /// one must always add, choosing between the identity and the point.
            ///
            /// For the two bit case, we instead have a set of [P'*0,P'*1,P'*2,P'*3]
            /// where P' = P*2^(2*i). Note that P'*0 is always the identity, and can be
            /// omitted from the table.
            ///
            /// This approach expands similarly for the higher window sizes. The
            /// tradeoff becomes an issue of table size (and precomputation cost)
            /// versus the number of additions in the online phase.
            ///
            /// At larger window sizes, extracting the needed element from the table in
            /// constant time becomes the dominating cost.
            struct [<$affine PrecomputedTable>] {
                tbl: Vec<ic_bls12_381::$affine>,
            }

            impl [<$affine PrecomputedTable>] {
                /// The size of the windows
                ///
                /// This algorithm uses just `SUBGROUP_BITS/WINDOW_BITS` additions in
                /// the online phase, at the cost of storing a table of size
                /// `(SUBGROUP_BITS + WINDOW_BITS - 1)/WINDOW_BITS * (1 << WINDOW_BITS - 1)`
                ///
                /// This constant is configurable and can take values between 1 and 7
                /// (inclusive)
                ///
                /// | WINDOW_BITS | TABLE_SIZE | online additions |
                /// | ----------- | ---------- | ---------------- |
                /// |           1 |       255  |              255 |
                /// |           2 |       384  |              128 |
                /// |           3 |       595  |               85 |
                /// |           4 |       960  |               64 |
                /// |           5 |      1581  |               51 |
                /// |           6 |      2709  |               43 |
                /// |           7 |      4699  |               37 |
                ///
                const WINDOW_BITS: usize = 4;

                /// The bit length of the BLS12-381 subgroup
                const SUBGROUP_BITS: usize = 255;

                // A bitmask of all 1s that is WINDOW_BITS long
                const WINDOW_MASK: u8 = (1 << Self::WINDOW_BITS) - 1;

                // The total number of windows in a scalar
                const WINDOWS : usize = (Self::SUBGROUP_BITS + Self::WINDOW_BITS - 1) / Self::WINDOW_BITS;

                // We must select from 2^WINDOW_BITS elements in each table
                // group. However one element of the table group is always the
                // identity, and so can be omitted, which is the reason for the
                // subtraction by 1 here.
                const WINDOW_ELEMENTS : usize = (1 << Self::WINDOW_BITS) - 1;

                // The total size of the table we will use
                const TABLE_SIZE: usize = Self::WINDOW_ELEMENTS * Self::WINDOWS;

                /// Precompute a table for fast multiplication
                fn new(pt: &$affine) -> Self {
                    let mut ptbl = vec![ic_bls12_381::$projective::identity(); Self::TABLE_SIZE];

                    let mut accum = ic_bls12_381::$projective::from(pt.value);

                    for i in 0..Self::WINDOWS {
                        let tbl_i = &mut ptbl[Self::WINDOW_ELEMENTS*i..Self::WINDOW_ELEMENTS*(i+1)];

                        tbl_i[0] = accum;
                        for j in 1..Self::WINDOW_ELEMENTS {
                            // Our table indexes are off by one due to the ommitted
                            // identity element. So here we are checking if we are
                            // about to compute a point that is a doubling of a point
                            // we have previously computed. If so we can compute it
                            // using a (faster) doubling rather than using addition.

                            tbl_i[j] = if j % 2 == 1 {
                                tbl_i[j / 2].double()
                            } else {
                                tbl_i[j - 1] + tbl_i[0]
                            };
                        }

                        // move on to the next power
                        accum = tbl_i[Self::WINDOW_ELEMENTS/2].double();
                    }

                    // batch convert the table to affine form, so we can use mixed addition
                    // in the online phase.
                    let mut tbl = vec![ic_bls12_381::$affine::identity(); Self::TABLE_SIZE];
                    <ic_bls12_381::$projective>::batch_normalize(&ptbl, &mut tbl);

                    Self { tbl }
                }


                /// Perform scalar multiplication using the precomputed table
                fn mul(&self, scalar: &Scalar) -> $projective {
                    let s = scalar.serialize();

                    let mut accum = <ic_bls12_381::$projective>::identity();

                    for i in 0..Self::WINDOWS {
                        let tbl_for_i = &self.tbl[Self::WINDOW_ELEMENTS*i..Self::WINDOW_ELEMENTS*(i+1)];

                        let b = Self::get_window(&s, Self::WINDOW_BITS*i);
                        accum += Self::ct_select(tbl_for_i, b as usize);
                    }

                    <$projective>::new(accum)
                }

                // Extract a WINDOW_BITS sized window out of s, depending on offset.
                #[inline(always)]
                fn get_window(s: &[u8], offset: usize) -> u8 {
                    const BITS_IN_BYTE: usize = 8;

                    let shift = offset % BITS_IN_BYTE;
                    let byte_offset = s.len() - 1 - (offset / BITS_IN_BYTE);

                    let w0 = s[byte_offset];

                    let single_byte_window =
                        shift <= (BITS_IN_BYTE - Self::WINDOW_BITS) || byte_offset == 0;

                    let bits = if single_byte_window {
                        // If we can get the window out of single byte, do so
                        (w0 >> shift)
                    } else {
                        // Otherwise we must join two bytes and extract the result
                        let w1 = s[byte_offset - 1];
                        ((w0 >> shift) | (w1 << (BITS_IN_BYTE - shift)))
                    };

                    bits & Self::WINDOW_MASK
                }

                // Constant time table lookup
                //
                // This version is specifically adapted to this algorithm. If
                // index is zero, then it returns the identity element. Otherwise
                // it returns from[index-1].
                #[inline(always)]
                fn ct_select(from: &[ic_bls12_381::$affine], index: usize) -> ic_bls12_381::$affine {
                    use subtle::{ConditionallySelectable, ConstantTimeEq};

                    let mut val = ic_bls12_381::$affine::identity();

                    let index = index.wrapping_sub(1);
                    for v in 0..from.len() {
                        val.conditional_assign(&from[v], usize::ct_eq(&v, &index));
                    }

                    val
                }
            }
        }

        /// An element of the group in affine form
        #[derive(Clone)]
        pub struct $affine {
            value: ic_bls12_381::$affine,
            precomputed: Option<Arc<paste! { [<$affine PrecomputedTable>] }>>,
        }

        impl Eq for $affine {}

        impl PartialEq for $affine {
            fn eq(&self, other: &Self) -> bool {
                self.value == other.value
            }
        }

        impl Zeroize for $affine {
            fn zeroize(&mut self) {
                self.value.zeroize();
                self.precomputed = None;
            }
        }

        impl Drop for $affine {
            fn drop(&mut self) {
                self.zeroize();
            }
        }

        impl $affine {
            /// The size in bytes of this type
            pub const BYTES: usize = $size;

            /// Create a struct from the inner type
            pub(crate) fn new(value: ic_bls12_381::$affine) -> Self {
                Self { value, precomputed: None }
            }

            /// Create a struct from the inner type, with precomputation
            pub(crate) fn new_with_precomputation(value: ic_bls12_381::$affine) -> Self {
                let mut s = Self::new(value);
                s.precompute();
                s
            }

            /// Precompute values for multiplication
            pub fn precompute(&mut self) {
                if self.precomputed.is_some() {
                    // already precomputed, no need to redo
                    return;
                }

                let tbl = <paste! { [<$affine PrecomputedTable>] }>::new(self);
                self.precomputed = Some(Arc::new(tbl));
            }

            /// Perform point multiplication
            pub(crate) fn mul_dispatch(&self, scalar: &Scalar) -> $projective {
                if let Some(ref tbl) = self.precomputed {
                    tbl.mul(scalar)
                } else {
                    <$projective>::from(self).windowed_mul(scalar)
                }
            }

            /// Return the inner value
            pub(crate) fn inner(&self) -> &ic_bls12_381::$affine {
                &self.value
            }

            /// Return the identity element in this group
            pub fn identity() -> Self {
                Self::new(ic_bls12_381::$affine::identity())
            }

            /// Return the generator element in this group
            pub fn generator() -> &'static Self {
                paste! { &[<$affine:upper _GENERATOR>] }
            }

            /// Hash into the group
            ///
            /// This follows draft-irtf-cfrg-hash-to-curve-16 using the
            /// BLS12381G1_XMD:SHA-256_SSWU_RO_ or
            /// BLS12381G2_XMD:SHA-256_SSWU_RO_ suite.
            ///
            /// # Arguments
            /// * `domain_sep` - some protocol specific domain seperator
            /// * `input` - the input which will be hashed
            pub fn hash(domain_sep: &[u8], input: &[u8]) -> Self {
                $projective::hash(domain_sep, input).into()
            }

            /// Hash into the group, returning a point with precomputations
            ///
            /// This follows draft-irtf-cfrg-hash-to-curve-16 using the
            /// BLS12381G1_XMD:SHA-256_SSWU_RO_ or
            /// BLS12381G2_XMD:SHA-256_SSWU_RO_ suite.
            ///
            /// # Arguments
            /// * `domain_sep` - some protocol specific domain seperator
            /// * `input` - the input which will be hashed
            pub fn hash_with_precomputation(domain_sep: &[u8], input: &[u8]) -> Self {
                let mut pt = Self::hash(domain_sep, input);
                pt.precompute();
                pt
            }

            /// Deserialize a point (compressed format only)
            ///
            /// This version verifies that the decoded point is within the prime order
            /// subgroup, and is safe to call on untrusted inputs.
            pub fn deserialize<B: AsRef<[u8]>>(bytes: &B) -> Result<Self, PairingInvalidPoint> {
                let bytes : &[u8; Self::BYTES] = bytes.as_ref()
                    .try_into()
                    .map_err(|_| PairingInvalidPoint::InvalidPoint)?;
                let pt = ic_bls12_381::$affine::from_compressed(bytes);
                ctoption_ok_or!(pt, PairingInvalidPoint::InvalidPoint)
            }

            /// Deserialize multiple point (compressed format only)
            ///
            /// This version verifies that the decoded point is within the prime order
            /// subgroup, and is safe to call on untrusted inputs. It returns Ok only
            /// if all of the provided bytes represent a valid point.
            pub fn batch_deserialize<B: AsRef<[u8]>>(inputs: &[B]) -> Result<Vec<Self>, PairingInvalidPoint> {
                let mut r = Vec::with_capacity(inputs.len());
                for input in inputs {
                    r.push(Self::deserialize(input)?);
                }
                Ok(r)
            }

            /// Deserialize a point (compressed format only), trusted bytes edition
            ///
            /// As only compressed format is accepted, it is not possible to
            /// create a point which is not on the curve. However it is possible
            /// using this function to create a point which is not within the
            /// prime-order subgroup. This can be detected by calling is_torsion_free
            pub fn deserialize_unchecked<B: AsRef<[u8]>>(bytes: &B) -> Result<Self, PairingInvalidPoint> {
                let bytes : &[u8; Self::BYTES] = bytes.as_ref()
                    .try_into()
                    .map_err(|_| PairingInvalidPoint::InvalidPoint)?;
                let pt = ic_bls12_381::$affine::from_compressed_unchecked(bytes);
                ctoption_ok_or!(pt, PairingInvalidPoint::InvalidPoint)
            }

            /// Serialize this point in compressed format
            pub fn serialize(&self) -> [u8; Self::BYTES] {
                self.value.to_compressed()
            }

            /// Serialize a point in compressed format in some specific type
            pub fn serialize_to<T: From<[u8; Self::BYTES]>>(&self) -> T {
                T::from(self.serialize())
            }

            /// Return true if this is the identity element
            pub fn is_identity(&self) -> bool {
                bool::from(self.value.is_identity())
            }

            /// Return true if this value is in the prime-order subgroup
            ///
            /// This will always be true unless the unchecked deserialization
            /// routine is used.
            pub fn is_torsion_free(&self) -> bool {
                bool::from(self.value.is_torsion_free())
            }

            /// Return the inverse of this point
            pub fn neg(&self) -> Self {
                use std::ops::Neg;
                Self::new(self.value.neg())
            }

            /// Batch multiplication
            pub fn batch_mul(&self, scalars: &[Scalar]) -> Vec<Self> {

                // It might be possible to optimize this function by taking advantage of
                // the fact that we are using the same point for several multiplications,
                // for example by using larger precomputed tables

                let mut result = Vec::with_capacity(scalars.len());
                for scalar in scalars {
                    result.push(self * scalar);
                }
                $projective::batch_normalize(&result)
            }
        }

        paste! {
            lazy_static::lazy_static! {
                static ref [<$projective:upper _GENERATOR>] : $projective = $projective::new(ic_bls12_381::$projective::generator());
            }
        }

        /// An element of the group in projective form
        #[derive(Clone, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
        pub struct $projective {
            value: ic_bls12_381::$projective
        }

        impl $projective {
            /// The size in bytes of this type
            pub const BYTES: usize = $size;

            /// Create a new struct from the inner type
            pub(crate) fn new(value: ic_bls12_381::$projective) -> Self {
                Self { value }
            }

            /// Return the inner value
            pub(crate) fn inner(&self) -> &ic_bls12_381::$projective {
                &self.value
            }

            /// Constant time selection
            ///
            /// Equivalent to from[index] except avoids leaking the index
            /// through side channels.
            ///
            /// If index is out of range, returns the identity element
            pub(crate) fn ct_select(from: &[Self], index: usize) -> Self {
                use subtle::{ConditionallySelectable, ConstantTimeEq};
                let mut val = ic_bls12_381::$projective::identity();

                for v in 0..from.len() {
                    val.conditional_assign(from[v].inner(), usize::ct_eq(&v, &index));
                }

                Self::new(val)
            }

            /// Return the doubling of this point
            pub fn double(&self) -> Self {
                Self::new(self.value.double())
            }

            /// Sum some points
            pub fn sum(pts: &[Self]) -> Self {
                let mut sum = ic_bls12_381::$projective::identity();
                for pt in pts {
                    sum += pt.inner();
                }
                Self::new(sum)
            }

            /// Deserialize a point (compressed format only)
            ///
            /// This version verifies that the decoded point is within the prime order
            /// subgroup, and is safe to call on untrusted inputs.
            pub fn deserialize<B: AsRef<[u8]>>(bytes: &B) -> Result<Self, PairingInvalidPoint> {
                let pt = $affine::deserialize(bytes)?;
                Ok(pt.into())
            }

            /// Serialize a point in compressed format in some specific type
            pub fn serialize_to<T: From<[u8; Self::BYTES]>>(&self) -> T {
                T::from(self.serialize())
            }

            /// Deserialize a point (compressed format only), trusted bytes edition
            ///
            /// As only compressed format is accepted, it is not possible to
            /// create a point which is not on the curve. However it is possible
            /// using this function to create a point which is not within the
            /// prime-order subgroup. This can be detected by calling is_torsion_free
            pub fn deserialize_unchecked<B: AsRef<[u8]>>(bytes: &B) -> Result<Self, PairingInvalidPoint> {
                let pt = $affine::deserialize_unchecked(bytes)?;
                Ok(pt.into())
            }

            /// Serialize this point in compressed format
            pub fn serialize(&self) -> [u8; Self::BYTES] {
                $affine::from(self).serialize()
            }

            /// Return the identity element in this group
            pub fn identity() -> Self {
                Self::new(ic_bls12_381::$projective::identity())
            }

            /// Return a list of n elements all of which are the identity element
            pub(crate) fn identities(count: usize) -> Vec<Self> {
                let mut v = Vec::with_capacity(count);
                for _ in 0..count {
                    v.push(Self::identity());
                }
                v
            }

            /// Return the generator element in this group
            pub fn generator() -> &'static Self {
                paste! { &[<$projective:upper _GENERATOR>] }
            }

            /// Hash into the group
            ///
            /// This follows draft-irtf-cfrg-hash-to-curve-16 using the
            /// BLS12381G1_XMD:SHA-256_SSWU_RO_ or
            /// BLS12381G2_XMD:SHA-256_SSWU_RO_ suite.
            ///
            /// # Arguments
            /// * `domain_sep` - some protocol specific domain seperator
            /// * `input` - the input which will be hashed
            pub fn hash(domain_sep: &[u8], input: &[u8]) -> Self {
                let pt =
                    <ic_bls12_381::$projective as HashToCurve<ExpandMsgXmd<sha2::Sha256>>>::hash_to_curve(
                        input, domain_sep,
                    );
                Self::new(pt)
            }

            /// Return true if this is the identity element
            pub fn is_identity(&self) -> bool {
                bool::from(self.value.is_identity())
            }

            /// Return the inverse of this point
            pub fn neg(&self) -> Self {
                use std::ops::Neg;
                Self::new(self.value.neg())
            }

            /// Convert this point to affine format
            pub fn to_affine(&self) -> $affine {
                $affine::new(self.value.into())
            }

            /// Convert a group of points into affine format
            pub fn batch_normalize(points: &[Self]) -> Vec<$affine> {
                let mut inner_points = Vec::with_capacity(points.len());
                for point in points {
                    inner_points.push(*point.inner());
                }

                let mut inner_affine = vec![ic_bls12_381::$affine::identity(); points.len()];
                ic_bls12_381::$projective::batch_normalize(&inner_points, &mut inner_affine);

                let mut output = Vec::with_capacity(points.len());
                for point in inner_affine {
                    output.push($affine::new(point));
                }
                output
            }
        }

        impl std::ops::Mul<&Scalar> for &$affine {
            type Output = $projective;

            fn mul(self, scalar: &Scalar) -> $projective {
                self.mul_dispatch(scalar)
            }
        }

        impl std::ops::Mul<&Scalar> for $affine {
            type Output = $projective;

            fn mul(self, scalar: &Scalar) -> $projective {
                self.mul_dispatch(&scalar)
            }
        }

        impl std::ops::Mul<Scalar> for &$affine {
            type Output = $projective;

            fn mul(self, scalar: Scalar) -> $projective {
                self * &scalar
            }
        }

        impl std::ops::Mul<Scalar> for $affine {
            type Output = $projective;

            fn mul(self, scalar: Scalar) -> $projective {
                &self * &scalar
            }
        }

        impl std::convert::From<$affine> for $projective {
            fn from(pt: $affine) -> Self {
                Self::new(pt.inner().into())
            }
        }

        impl std::convert::From<&$affine> for $projective {
            fn from(pt: &$affine) -> Self {
                Self::new(pt.inner().into())
            }
        }

        impl std::convert::From<$projective> for $affine {
            fn from(pt: $projective) -> Self {
                Self::new(pt.inner().into())
            }
        }

        impl std::convert::From<&$projective> for $affine {
            fn from(pt: &$projective) -> Self {
                Self::new(pt.inner().into())
            }
        }

    }
}

// declare the impl for the mul2 table struct
macro_rules! declare_mul2_table_impl {
    ($projective:ty, $tbl_typ:ident, $window:expr) => {
        /// Table for storing linear combinations of two points.
        /// It is stored as a vector to reduce the amount of indirection for accessing cells.
        /// A table can be computed by calling the `compute_mul2_tbl` function of the corresponding
        /// projective `struct`, e.g., `G2Projective::mul2_prepared(...)`.
        impl $tbl_typ {
            // Compute the column offset in the vector from the column index.
            pub(crate) fn col(i: usize) -> usize {
                i
            }

            // Compute the row offset in the vector from the row index.
            pub(crate) fn row(i: usize) -> usize {
                // Configurable window size: an be in 1..=8
                type Window = WindowInfo<$window>;
                i << Window::SIZE
            }

            /// Multiscalar multiplication (aka sum-of-products)
            ///
            /// This table contains linear combinations of points x and y
            /// that allow for fast multiplication with scalars.
            /// The result of the computation is equivalent to x*a + y*b.
            /// It is intended and beneficial to call this function on multiple
            /// scalar pairs without recomputing this table.
            /// If `mul2` is called only once, consider using the associated
            /// `mul2` function of the respective projective struct, which
            /// computes a smaller mul2 table on the fly and might thus be more efficient.
            ///
            /// Uses the Simultaneous 2w-Ary Method following Section 2.1 of
            /// <https://www.bmoeller.de/pdf/multiexp-sac2001.pdf>
            ///
            /// This function is intended to work in constant time, and not
            /// leak information about the inputs.
            pub fn mul2(&self, a: &Scalar, b: &Scalar) -> $projective {
                // Configurable window size: can be in 1..=8
                type Window = WindowInfo<$window>;

                let s1 = a.serialize();
                let s2 = b.serialize();

                let mut accum = <$projective>::identity();

                for i in 0..Window::WINDOWS {
                    // skip on first iteration: doesn't leak secrets as index is public
                    if i > 0 {
                        for _ in 0..Window::SIZE {
                            accum = accum.double();
                        }
                    }

                    let w1 = Window::extract(&s1, i);
                    let w2 = Window::extract(&s2, i);
                    let window = $tbl_typ::col(w1 as usize) + $tbl_typ::row(w2 as usize);

                    accum += <$projective>::ct_select(&self.0, window);
                }

                accum
            }
        }
    };
}

macro_rules! declare_compute_mul2_table_inline {
    ($projective:ty, $tbl_typ:ident, $window_size:expr, $x:expr, $y:expr) => {{
        // Configurable window size: can be in 1..=8
        type Window = WindowInfo<$window_size>;

        // Derived constants
        const TABLE_SIZE: usize = Window::ELEMENTS * Window::ELEMENTS;

        /*
        A table which can be viewed as a 2^WINDOW_SIZE x 2^WINDOW_SIZE matrix

        Each element is equal to a small linear combination of x and y:

        tbl[(yi:xi)] = x*xi + y*yi

        where xi is the lowest bits of the index and yi is the upper bits.  Each
        xi and yi is WINDOW_SIZE bits long (and thus at most 2^WINDOW_SIZE).

        We build up the table incrementally using additions and doubling, to
        avoid the cost of full scalar mul.
         */
        let mut tbl = <$projective>::identities(TABLE_SIZE);

        // Precompute the table (tbl[0] is left as the identity)
        for i in 1..TABLE_SIZE {
            // The indexing here depends just on i, which is a public loop index

            let xi = i % Window::ELEMENTS;
            let yi = (i >> Window::SIZE) % Window::ELEMENTS;

            if xi % 2 == 0 && yi % 2 == 0 {
                tbl[i] = tbl[i / 2].double();
            } else if xi > 0 && yi > 0 {
                tbl[i] = &tbl[$tbl_typ::col(xi)] + &tbl[$tbl_typ::row(yi)];
            } else if xi > 0 {
                tbl[i] = &tbl[$tbl_typ::col(xi - 1)] + $x;
            } else if yi > 0 {
                tbl[i] = &tbl[$tbl_typ::row(yi - 1)] + $y;
            }
        }

        $tbl_typ(tbl)
    }};
}

macro_rules! declare_mul2_impl_for {
    ( $projective:ty, $tbl_typ:ident, $small_window_size:expr, $big_window_size:expr ) => {
        paste! {
            /// Contains a small precomputed table with linear combinations of two points that
            /// can be used for faster mul2 computation. This table is called small because its
            /// parameters are optimized for computation on the fly, meaning that it this table
            /// is computed for each mul2 call without further optimizations.
            pub(crate) struct [< Small $tbl_typ >](Vec<$projective>);
            declare_mul2_table_impl!($projective, [< Small $tbl_typ >], $small_window_size);

            /// Contains a small precomputed table with linear combinations of two points that
            /// can be used for faster mul2 computation. This table is called large because
            /// its parameters are optimized for the best trade-off for pre-computing the table
            /// once and using it for multiplication of the points with multiple scalar pairs.
            /// For further information, see the rustdoc of `mul2` and `compute_mul2_tbl`.
            pub struct $tbl_typ(Vec<$projective>);
            declare_mul2_table_impl!($projective, $tbl_typ, $big_window_size);

            impl $projective {
                /// Multiscalar multiplication (aka sum-of-products)
                ///
                /// Equivalent to x*a + y*b
                ///
                /// Uses the Simultaneous 2w-Ary Method following Section 2.1 of
                /// <https://www.bmoeller.de/pdf/multiexp-sac2001.pdf>
                ///
                /// This function is intended to work in constant time, and not
                /// leak information about the inputs.
                pub fn mul2(x: &Self, a: &Scalar, y: &Self, b: &Scalar) -> Self {
                    let tbl = Self::compute_small_mul2_tbl(x, y);
                    tbl.mul2(a, b)
                }

                /// Compute a small mul2 table for computing mul2 on the fly, i.e.,
                /// without amortizing the cost of the table computation by
                /// reusing it (calling mul2) on multiple scalar pairs.
                fn compute_small_mul2_tbl(x: &Self, y: &Self) -> [< Small $tbl_typ >] {
                    declare_compute_mul2_table_inline!($projective, [< Small $tbl_typ >], $small_window_size, x, y)
                }

                /// Compute a mul2 table that contains linear combinations of `x` and `y`,
                /// which is intended to be used for multiple mul2 calls with the same `x` and `y`
                /// but different scalar pairs. To call `mul2` only once, consider calling
                /// it directly, which might be more efficient.
                pub fn compute_mul2_tbl(x: &Self, y: &Self) -> $tbl_typ {
                    declare_compute_mul2_table_inline!($projective, $tbl_typ, $big_window_size, x, y)
                }
            }
        }
    };
}

macro_rules! declare_muln_vartime_impl_for {
    ( $typ:ty, $window:expr ) => {
        impl $typ {
            /// Multiscalar multiplication using Pippenger's algorithm
            ///
            /// Equivalent to p1*s1 + p2*s2 + p3*s3 + ... + pn*sn,
            /// where `n = min(points.len(), scalars.len())`.
            ///
            /// Returns the identity element if terms is empty.
            ///
            /// Warning: this function leaks information about the scalars via
            /// memory-based side channels. Do not use this function with secret
            /// scalars.
            pub fn muln_vartime(points: &[Self], scalars: &[Scalar]) -> Self {
                // Configurable window size: can be in 1..=8
                type Window = WindowInfo<$window>;

                let count = std::cmp::min(points.len(), scalars.len());

                let mut windows = Vec::with_capacity(count);
                for s in scalars {
                    let sb = s.serialize();

                    let mut window = [0u8; Window::WINDOWS];
                    for i in 0..Window::WINDOWS {
                        window[i] = Window::extract(&sb, i);
                    }
                    windows.push(window);
                }

                let mut accum = Self::identity();

                let mut buckets = Self::identities(Window::ELEMENTS);

                for i in 0..Window::WINDOWS {
                    let mut max_bucket = 0;
                    for j in 0..count {
                        let bucket_index = windows[j][i] as usize;
                        if bucket_index > 0 {
                            buckets[bucket_index] += &points[j];
                            max_bucket = std::cmp::max(max_bucket, bucket_index);
                        }
                    }

                    if i > 0 {
                        for _ in 0..Window::SIZE {
                            accum = accum.double();
                        }
                    }

                    let mut t = Self::identity();

                    for j in (1..=max_bucket).rev() {
                        t += &buckets[j];
                        accum += &t;
                        buckets[j] = Self::identity();
                    }
                }

                accum
            }
        }
    };
}

macro_rules! declare_muln_vartime_affine_impl_for {
    ( $proj:ty, $affine:ty ) => {
        impl $proj {
            /// Multiscalar multiplication
            ///
            /// Equivalent to p1*s1 + p2*s2 + p3*s3 + ... + pn*sn,
            /// where `n = min(points.len(), scalars.len())`.
            ///
            /// Returns the identity element if terms is empty.
            ///
            /// Warning: this function leaks information about the scalars via
            /// memory-based side channels. Do not use this function with secret
            /// scalars.
            pub fn muln_affine_vartime(points: &[$affine], scalars: &[Scalar]) -> Self {
                let count = std::cmp::min(points.len(), scalars.len());
                let mut proj_points = Vec::with_capacity(count);

                for i in 0..count {
                    proj_points.push(<$proj>::from(&points[i]));
                }

                Self::muln_vartime(&proj_points[..], scalars)
            }
        }
    };
}

macro_rules! declare_windowed_scalar_mul_ops_for {
    ( $typ:ty, $window:expr ) => {
        impl $typ {
            pub(crate) fn windowed_mul(&self, scalar: &Scalar) -> Self {
                // Configurable window size: can be in 1..=8
                type Window = WindowInfo<$window>;

                // Derived constants
                const TABLE_SIZE: usize = Window::ELEMENTS;

                let mut tbl = Self::identities(TABLE_SIZE);

                for i in 1..TABLE_SIZE {
                    tbl[i] = if i % 2 == 0 {
                        tbl[i / 2].double()
                    } else {
                        &tbl[i - 1] + self
                    };
                }

                let s = scalar.serialize();

                let mut accum = Self::identity();

                for i in 0..Window::WINDOWS {
                    // skip on first iteration: doesn't leak secrets as index is public
                    if i > 0 {
                        for _ in 0..Window::SIZE {
                            accum = accum.double();
                        }
                    }

                    let w = Window::extract(&s, i);
                    accum += Self::ct_select(&tbl, w as usize);
                }

                accum
            }
        }

        impl std::ops::Mul<&Scalar> for &$typ {
            type Output = $typ;
            fn mul(self, scalar: &Scalar) -> $typ {
                self.windowed_mul(scalar)
            }
        }

        impl std::ops::Mul<&Scalar> for $typ {
            type Output = $typ;
            fn mul(self, scalar: &Scalar) -> $typ {
                &self * scalar
            }
        }

        impl std::ops::Mul<Scalar> for &$typ {
            type Output = $typ;
            fn mul(self, scalar: Scalar) -> Self::Output {
                self * &scalar
            }
        }

        impl std::ops::Mul<Scalar> for $typ {
            type Output = $typ;
            fn mul(self, scalar: Scalar) -> Self::Output {
                &self * &scalar
            }
        }

        impl std::ops::MulAssign<Scalar> for $typ {
            fn mul_assign(&mut self, other: Scalar) {
                *self = self.windowed_mul(&other);
            }
        }

        impl std::ops::MulAssign<&Scalar> for $typ {
            fn mul_assign(&mut self, other: &Scalar) {
                *self = self.windowed_mul(other);
            }
        }
    };
}

define_affine_and_projective_types!(G1Affine, G1Projective, 48);
declare_addsub_ops_for!(G1Projective);
declare_mixed_addition_ops_for!(G1Projective, G1Affine);
declare_windowed_scalar_mul_ops_for!(G1Projective, 4);
declare_mul2_impl_for!(G1Projective, G1Mul2Table, 2, 3);
declare_muln_vartime_impl_for!(G1Projective, 3);
declare_muln_vartime_affine_impl_for!(G1Projective, G1Affine);
impl_debug_using_serialize_for!(G1Affine);
impl_debug_using_serialize_for!(G1Projective);

define_affine_and_projective_types!(G2Affine, G2Projective, 96);
declare_addsub_ops_for!(G2Projective);
declare_mixed_addition_ops_for!(G2Projective, G2Affine);
declare_windowed_scalar_mul_ops_for!(G2Projective, 4);
declare_mul2_impl_for!(G2Projective, G2Mul2Table, 2, 3);
declare_muln_vartime_impl_for!(G2Projective, 3);
declare_muln_vartime_affine_impl_for!(G2Projective, G2Affine);
impl_debug_using_serialize_for!(G2Affine);
impl_debug_using_serialize_for!(G2Projective);

/// An element of the group Gt
#[derive(Clone, Debug, Eq, PartialEq, Zeroize, ZeroizeOnDrop)]
pub struct Gt {
    value: ic_bls12_381::Gt,
}

lazy_static::lazy_static! {
    static ref GT_GENERATOR : Gt = Gt::new(ic_bls12_381::Gt::generator());
}

impl Gt {
    /// The size in bytes of this type
    pub const BYTES: usize = 576;

    /// Create a new Gt from the inner type
    pub(crate) fn new(value: ic_bls12_381::Gt) -> Self {
        Self { value }
    }

    pub(crate) fn inner(&self) -> &ic_bls12_381::Gt {
        &self.value
    }

    /// Constant time selection
    ///
    /// Equivalent to from[index] except avoids leaking the index
    /// through side channels.
    ///
    /// If index is out of range, returns the identity element
    pub(crate) fn ct_select(from: &[Self], index: usize) -> Self {
        use subtle::{ConditionallySelectable, ConstantTimeEq};
        let mut val = ic_bls12_381::Gt::identity();

        for v in 0..from.len() {
            val.conditional_assign(from[v].inner(), usize::ct_eq(&v, &index));
        }

        Self::new(val)
    }

    pub(crate) fn conditional_select(a: &Self, b: &Self, choice: subtle::Choice) -> Self {
        use subtle::ConditionallySelectable;
        Self::new(ic_bls12_381::Gt::conditional_select(
            a.inner(),
            b.inner(),
            choice,
        ))
    }

    /// Return the identity element in the group
    pub fn identity() -> Self {
        Self::new(ic_bls12_381::Gt::identity())
    }

    /// Return a vector of the identity element
    pub(crate) fn identities(count: usize) -> Vec<Self> {
        let mut v = Vec::with_capacity(count);
        for _ in 0..count {
            v.push(Self::identity());
        }
        v
    }

    /// Return the generator element in the group
    pub fn generator() -> &'static Self {
        &GT_GENERATOR
    }

    /// Compute the pairing function e(g1,g2) -> gt
    pub fn pairing(g1: &G1Affine, g2: &G2Affine) -> Self {
        Self::new(ic_bls12_381::pairing(&g1.value, &g2.value))
    }

    /// Perform multi-pairing computation
    ///
    /// This is equivalent to computing the pairing from each element of
    /// `terms` then summing the result.
    pub fn multipairing(terms: &[(&G1Affine, &G2Prepared)]) -> Self {
        let mut inners = Vec::with_capacity(terms.len());
        for (g1, g2) in terms {
            inners.push((g1.inner(), g2.inner()));
        }

        Self::new(ic_bls12_381::multi_miller_loop(&inners).final_exponentiation())
    }

    /// Return true if this is the identity element
    pub fn is_identity(&self) -> bool {
        bool::from(self.value.is_identity())
    }

    /// Return the additive inverse of this Gt
    pub fn neg(&self) -> Self {
        use std::ops::Neg;
        Self::new(self.value.neg())
    }

    /// Return the doubling of this element
    pub fn double(&self) -> Self {
        Self::new(self.value.double())
    }

    /// Return some arbitrary bytes which represent this Gt element
    ///
    /// These are not deserializable, and serve only to uniquely identify
    /// the group element.
    pub fn tag(&self) -> [u8; Self::BYTES] {
        self.value.to_bytes()
    }

    /// Return a hash value of this element suitable for linear search
    ///
    /// # Warning
    ///
    /// This function is a perfect hash function (ie, has no collisions) for the
    /// set of elements gt*{0..2**16-1}, which is what is used to represent
    /// ciphertext elements in the NIDKG. It is not useful in other contexts.
    ///
    /// This function is not stable over time; it may change in the future.
    /// Do not serialize this value, or use it as an index in storage.
    pub fn short_hash_for_linear_search(&self) -> u32 {
        fn extract4(tag: &[u8], idx: usize) -> u32 {
            let mut fbytes = [0u8; 4];
            fbytes.copy_from_slice(&tag[idx..idx + 4]);
            u32::from_le_bytes(fbytes)
        }

        let tag = self.tag();
        extract4(&tag, 0) ^ extract4(&tag, 32)
    }

    /// Return the result of g*val where g is the standard generator
    ///
    /// This function avoids leaking val through timing side channels,
    /// since it is used when decrypting NIDKG dealings.
    pub fn g_mul_u16(val: u16) -> Self {
        let g = Gt::generator().clone();
        let mut r = Gt::identity();

        for b in 0..16 {
            if b > 0 {
                r = r.double();
            }

            let choice = subtle::Choice::from(((val >> (15 - b)) as u8) & 1);
            r = Self::conditional_select(&r, &(&r + &g), choice);
        }

        r
    }
}

declare_addsub_ops_for!(Gt);
declare_windowed_scalar_mul_ops_for!(Gt, 4);

/// An element of the group G2 prepared for the Miller loop
#[derive(Clone, Debug)]
pub struct G2Prepared {
    value: ic_bls12_381::G2Prepared,
}

lazy_static::lazy_static! {
    static ref G2PREPARED_G : G2Prepared = G2Affine::generator().into();
    static ref G2PREPARED_NEG_G : G2Prepared = G2Affine::generator().neg().into();
}

impl G2Prepared {
    /// Create a new G2Prepared from the inner type
    pub(crate) fn new(value: ic_bls12_381::G2Prepared) -> Self {
        Self { value }
    }

    pub(crate) fn inner(&self) -> &ic_bls12_381::G2Prepared {
        &self.value
    }

    /// Return the generator element in the group
    pub fn generator() -> &'static Self {
        &G2PREPARED_G
    }

    /// Return the inverse of the generator element in the group
    pub fn neg_generator() -> &'static Self {
        &G2PREPARED_NEG_G
    }
}

impl From<&G2Affine> for G2Prepared {
    fn from(v: &G2Affine) -> Self {
        Self::new((*v.inner()).into())
    }
}

impl From<&G2Projective> for G2Prepared {
    fn from(v: &G2Projective) -> Self {
        Self::from(G2Affine::from(v))
    }
}

impl From<G2Affine> for G2Prepared {
    fn from(v: G2Affine) -> Self {
        Self::from(&v)
    }
}

impl From<G2Projective> for G2Prepared {
    fn from(v: G2Projective) -> Self {
        Self::from(&v)
    }
}

/// Perform BLS signature verification
///
/// The naive version of this function requires two pairings, but it
/// is possible to use optimizations to reduce this overhead somewhat.
pub fn verify_bls_signature(
    signature: &G1Affine,
    public_key: &G2Affine,
    message: &G1Affine,
) -> bool {
    // faster version of
    // Gt::pairing(&signature, &G2Affine::generator()) == Gt::pairing(&message, &public_key)

    let g2_gen = G2Prepared::neg_generator();
    let pub_key_prepared = G2Prepared::from(public_key);
    Gt::multipairing(&[(signature, g2_gen), (message, &pub_key_prepared)]).is_identity()
}

struct WindowInfo<const WINDOW_SIZE: usize> {}

impl<const WINDOW_SIZE: usize> WindowInfo<WINDOW_SIZE> {
    const SIZE: usize = WINDOW_SIZE;
    const WINDOWS: usize = (Scalar::BYTES * 8 + Self::SIZE - 1) / Self::SIZE;

    const MASK: u8 = 0xFFu8 >> (8 - Self::SIZE);
    const ELEMENTS: usize = 1 << Self::SIZE;

    #[inline(always)]
    /// * `bit_len` denotes the total bit size
    /// * `inverted_w` denotes the window index counting from the least significant part of the scalar
    fn window_bit_offset(inverted_w: usize) -> usize {
        (inverted_w * Self::SIZE) % 8
    }

    #[inline(always)]
    /// Extract a window from a serialized scalar value
    ///
    /// Treat the scalar as if it was a sequence of windows, each of WINDOW_SIZE bits,
    /// and return the `w`th one of them. For 8 bit windows, this is simply the byte
    /// value. For smaller windows this is some subset of a single byte.
    /// Note that `w=0` is the window corresponding to the largest value, i.e., if
    /// out scalar spans one byte and is equal to 10101111_2=207_10, then it first, say
    /// 4-bit, window will be 1010_2=10_10.
    ///
    /// Only window sizes in 1..=8 are supported.
    fn extract(scalar: &[u8], w: usize) -> u8 {
        assert!((1..=8).contains(&Self::SIZE));
        const BITS_IN_BYTE: usize = 8;

        // to compute the correct bit offset for bit lengths that are not a power of 2,
        // we need to start from the inverted value or otherwise we will have multiple options
        // for the offset.
        let inverted_w = Self::WINDOWS - w - 1;
        let bit_offset = Self::window_bit_offset(inverted_w);
        let byte_offset = Scalar::BYTES - 1 - (inverted_w * Self::SIZE) / 8;
        let target_byte = scalar[byte_offset];

        let no_overflow = bit_offset + Self::SIZE <= BITS_IN_BYTE;

        let non_overflow_bits = target_byte >> bit_offset;

        if no_overflow || byte_offset == 0 {
            // If we can get the window out of single byte, do so
            non_overflow_bits & Self::MASK
        } else {
            // Otherwise we must join two bytes and extract the result
            let prev_byte = scalar[byte_offset - 1];
            let overflow_bits = prev_byte << (BITS_IN_BYTE - bit_offset);
            (non_overflow_bits | overflow_bits) & Self::MASK
        }
    }
}
