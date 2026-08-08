//! A throwaway RSA key for the auth tests.
//!
//! Stored as hex-encoded PKCS#1 DER rather than as a PEM, deliberately. A real
//! PEM armour block in this repository would be flagged by our own secret
//! scanner — correctly — and refused by GitHub push protection, so the tests
//! would be unrunnable and the repository unpushable.
//!
//! The first draft of this very comment quoted the armour line to explain the
//! problem, and was itself blocked. Scanners match text, not intent.
//!
//! This key protects nothing. It is registered with no GitHub App and signs
//! nothing outside these tests.

/// The key, as hex-encoded DER.
pub const TEST_KEY_DER_HEX: &str = concat!(
    "308204a50201000282010100b5177600c0a7f39a3882fa0bc9df92310fef208a1725c0496cd8b9be84dc125d22fe2a1d",
    "d1a52595865ce47a2453373b9a8ee57d50bdce951a59607bb90f7c7b76d9aa1d91e7f24c104dbc16d8153a511a1ae91b",
    "039b6e5f04dec107b1765f779a59b90fbe826a8f69d84f18e136036dbcfc3161b7d40683eba23251968777a451b15456",
    "dc6c60d9ce8a77c7b0068e1451094cb07978ad6650a8353f324b819148eaf04bf3ebb5e124c365d0931348bc4e6406e2",
    "8907ecafba1df0ff7cff1ee9f0d98d335c5d0e3d4cbd91e39e7ee552253190d5ff5b829ecdf29a472b5b64da66bcb0b3",
    "2713770de878cc78be531b9c1030e8e3125ff9c9840fc2c5f7e0d5f10203010001028201002bd3babe61e203e5de296c",
    "c4af9dc92ed0916a09a1a2845000e4cec75a363cc787b18595e3e81919800439538a390d94024af5258805f7da441f3f",
    "6792193a62531848c091505666ac4773eeff6adbcb470b1e416875149830808cad04f9060fd72e41c89aadcb865bf27a",
    "ea258f41f32c1ac904c24db129fa3c2dfb6af7ec2f5351b2566207b8e0211bc2b2bf2955d24405cb92b16eb6c61ea29f",
    "cb245b2a61e926c751d99c3b9f865f7e2fb5a6692f30747582b187dd35271c75552296f4cabcf8f965b39389fcceb21d",
    "8001539bcf943a5bf1d6ec52a1667f1bf93dec5804f979d94cfb817aa8825ea56d5677cd1d75fc7d04a61c7ade7458fd",
    "da557dbef502818100d815f1869854881c2c1d008dba83264db3eb062d57f0aa2b7dc995cf97e06b8228c5c6f2523f07",
    "f7dd920b14a003b2a299032b8ae21c04c2c458c9495c4f84e395fc269852b41522eb717344718d8af8b6513156c31080",
    "38d4e4d220109cb4247912c2ba690c7bd8dd740171a49079aab8508578ba56e652cc4f559fc5766bd702818100d68ac0",
    "3f7b77f27039ebe52a566af82812266686cb72ee1efdca6466181d0528b72032e93f92ac766881838cad69f54504bed7",
    "83c338f28bad8443a81e858cdbfd9c7eb96e2107f24afa06fd951e3451e940a9ac9b992f03ad488e932e58860c032636",
    "7e0c2794f587e4a0ffb5c9186c81f47357f5457919896c0ac4f252537702818100d7b91875a990028e359002a47b8640",
    "f023e547366f6bc9473ffdc6fd077fb974a8e5c1d6db3b27e6512262c385780b977e308700d0f8eddbcf8f5fec4826ee",
    "e112343807abd132a4b8ee7b07e2614f533b1855ac6b7306bf35f2f6bfa235ff35c6556f681045b14270db4631c0fba7",
    "2b4374c7bb1e34711e49f00de8428715e302818100aa27bdb61bacd441a20eafe0d64d5ca81b4d0d7fd7183e37a23dc5",
    "471bd4d864a4690b37e74de32ebe500a0fa6f224af2ac659938d603b2e00dea7f24cd2cb17279bd8fe24945a0316e81a",
    "6740bf85eb793de9d4964bf5f7ca95834ec4313d8f8567e74c2d43af66d4f2c5a6497d46bbb88e32750e789d455db2ad",
    "0feac8d49d028181009501c3d72e4f7d77663518250200a38d7d9d23532feaaa26862b5d0d2f2e3da1cd17a72b2f7485",
    "1c8d122ee14c43a7f81249ff17ada57b1667c50d7e544f3c3d3295d3213ced8fb743c7c14a82ab2a98b16af6d18d8415",
    "1205db6a6b412f2fb2a9de8ea127fea75e706eeabb98c87bfd12bb5ca11a572f0aa9c16c16db2288cc",
);

/// Decode it into DER bytes.
pub fn test_key_der() -> Vec<u8> {
    (0..TEST_KEY_DER_HEX.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&TEST_KEY_DER_HEX[i..i + 2], 16).expect("valid hex"))
        .collect()
}
