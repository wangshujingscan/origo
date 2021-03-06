use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use ff::{PrimeField, PrimeFieldRepr};
use pairing::bls12_381::{Bls12, Fr, FrRepr};
use rlp::{self, DecoderError, Rlp, RlpStream};
use sapling_crypto::{
	jubjub::{edwards, Unknown},
	redjubjub::{PublicKey, Signature},
};
use std::io::{self, Read, Write};

use serialize::Vector;
use JUBJUB;

// π_A + π_B + π_C
pub const GROTH_PROOF_SIZE: usize = (48 + 96 + 48);
// π_A + π_A' + π_B + π_B' + π_C + π_C' + π_K + π_H
const PHGR_PROOF_SIZE: usize = (33 + 33 + 65 + 33 + 33 + 33 + 33 + 33);

const ZC_NUM_JS_INPUTS: usize = 2;
const ZC_NUM_JS_OUTPUTS: usize = 2;

const COIN: i64 = 1_0000_0000;
const MAX_MONEY: i64 = 21_000_000 * COIN;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Amount(pub i64);

impl Amount {
	// Read an Amount from a signed 64-bit little-endian integer.
	pub fn read_i64<R: Read>(mut reader: R, allow_negative: bool) -> io::Result<Self> {
		let amount = reader.read_i64::<LittleEndian>()?;
		if 0 <= amount && amount <= MAX_MONEY {
			Ok(Amount(amount))
		} else if allow_negative && -MAX_MONEY <= amount && amount < 0 {
			Ok(Amount(amount))
		} else {
			Err(io::Error::new(
				io::ErrorKind::InvalidData,
				if allow_negative {
					"Amount not in {-MAX_MONEY..MAX_MONEY}"
				} else {
					"Amount not in {0..MAX_MONEY}"
				},
			))
		}
	}

	// Read an Amount from an unsigned 64-bit little-endian integer.
	pub fn read_u64<R: Read>(mut reader: R) -> io::Result<Self> {
		let amount = reader.read_u64::<LittleEndian>()?;
		if amount <= MAX_MONEY as u64 {
			Ok(Amount(amount as i64))
		} else {
			Err(io::Error::new(
				io::ErrorKind::InvalidData,
				"Amount not in {0..MAX_MONEY}",
			))
		}
	}
}

#[derive(Debug)]
pub struct Script(pub Vec<u8>);

impl Script {
	pub fn read<R: Read>(mut reader: R) -> io::Result<Self> {
		let script = Vector::read(&mut reader, |r| r.read_u8())?;
		Ok(Script(script))
	}

	pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
		Vector::write(&mut writer, &self.0, |w, e| w.write_u8(*e))
	}
}

#[derive(Debug)]
pub struct OutPoint {
	hash: [u8; 32],
	n: u32,
}

impl OutPoint {
	pub fn read<R: Read>(mut reader: R) -> io::Result<Self> {
		let mut hash = [0; 32];
		reader.read_exact(&mut hash)?;
		let n = reader.read_u32::<LittleEndian>()?;
		Ok(OutPoint { hash, n })
	}

	pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
		writer.write_all(&self.hash)?;
		writer.write_u32::<LittleEndian>(self.n)
	}
}

#[derive(Debug)]
pub struct TxIn {
	pub prevout: OutPoint,
	script_sig: Script,
	pub sequence: u32,
}

impl TxIn {
	pub fn read<R: Read>(mut reader: &mut R) -> io::Result<Self> {
		let prevout = OutPoint::read(&mut reader)?;
		let script_sig = Script::read(&mut reader)?;
		let sequence = reader.read_u32::<LittleEndian>()?;

		Ok(TxIn {
			prevout,
			script_sig,
			sequence,
		})
	}

	pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
		self.prevout.write(&mut writer)?;
		self.script_sig.write(&mut writer)?;
		writer.write_u32::<LittleEndian>(self.sequence)
	}
}

#[derive(Debug)]
pub struct TxOut {
	value: Amount,
	script_pubkey: Script,
}

impl TxOut {
	pub fn read<R: Read>(mut reader: &mut R) -> io::Result<Self> {
		let value = Amount::read_i64(&mut reader, false)?;
		let script_pubkey = Script::read(&mut reader)?;

		Ok(TxOut {
			value,
			script_pubkey,
		})
	}

	pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
		writer.write_i64::<LittleEndian>(self.value.0)?;
		self.script_pubkey.write(&mut writer)
	}
}

/// SpendDescription is used by transaction in block. It is generated according
/// to SpendDescriptionInfo, which not include fields expsk and witness.
#[derive(Clone)]
pub struct SpendDescription {
	pub cv: edwards::Point<Bls12, Unknown>,
	pub anchor: Fr,
	pub nullifier: [u8; 32],
	pub rk: PublicKey<Bls12>,
	pub zkproof: [u8; GROTH_PROOF_SIZE],
	pub spend_auth_sig: Option<Signature>,
}

impl std::fmt::Debug for SpendDescription {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
		write!(
            f,
            "SpendDescription(cv = {:?}, anchor = {:?}, nullifier = {:?}, rk = {:?}, spend_auth_sig = {:?})",
            self.cv, self.anchor, self.nullifier, self.rk, self.spend_auth_sig
        )
	}
}

impl PartialEq for SpendDescription {
	fn eq(&self, other: &SpendDescription) -> bool {
		self.cv == other.cv
			&& self.anchor == other.anchor
			&& self.nullifier == other.nullifier
			&& self.rk == other.rk
			&& &self.zkproof[..] == &other.zkproof[..]
			&& self.spend_auth_sig == other.spend_auth_sig
	}
}
impl Eq for SpendDescription {}

impl rlp::Decodable for SpendDescription {
	fn decode(d: &Rlp) -> Result<Self, DecoderError> {
		if d.item_count()? != 6 {
			return Err(DecoderError::RlpIncorrectListLen);
		}
		let vec: Vec<u8> = d.val_at(0)?;
		let cv = edwards::Point::<Bls12, Unknown>::read(&mut &vec[..], &JUBJUB);
		let cv = match cv {
			Ok(v) => v,
			Err(_) => return Err(DecoderError::Custom("Invalid Point Rlp.")),
		};

		let mut fr_repr = FrRepr::default();
		let anchor_vec: Vec<u8> = d.val_at(1)?;
		if anchor_vec.len() != 32 {
			return Err(DecoderError::Custom("Invalid Anchor Rlp."));
		}
		fr_repr.read_le(&mut &anchor_vec[..]);
		let anchor = match Fr::from_repr(fr_repr) {
			Ok(fr) => fr,
			Err(_) => return Err(DecoderError::Custom("Invalid prime field for anchor.")),
		};

		let mut nullifier: [u8; 32] = [0; 32];
		let nullifier_vec: Vec<u8> = d.val_at(2)?;
		if nullifier_vec.len() != nullifier.len() {
			return Err(DecoderError::Custom("Invalid Nullifier Rlp."));
		}
		nullifier.copy_from_slice(&nullifier_vec);

		let vec: Vec<u8> = d.val_at(3)?;
		let rk = edwards::Point::<Bls12, Unknown>::read(&mut &vec[..], &JUBJUB);
		let rk = match rk {
			Ok(v) => v,
			Err(_) => return Err(DecoderError::Custom("Invalid Point Rlp.")),
		};
		let rk_pubkey = PublicKey::<Bls12>(rk);

		let mut zkproof: [u8; GROTH_PROOF_SIZE] = [0; GROTH_PROOF_SIZE];
		let zkproof_vec: Vec<u8> = d.val_at(4)?;
		if zkproof_vec.len() != zkproof.len() {
			return Err(DecoderError::Custom("Invalid Zkproof Rlp."));
		}
		zkproof.copy_from_slice(&zkproof_vec);

		let mut spend_auth_sig: Option<Signature> = None;
		let sig_rlp  = d.at(5)?;
		if !sig_rlp.is_empty() {
			spend_auth_sig = Some(d.val_at(5)?);
		}

		Ok(SpendDescription {
			cv,
			anchor,
			nullifier,
			rk: rk_pubkey,
			zkproof,
			spend_auth_sig,
		})
	}
}

impl rlp::Encodable for SpendDescription {
	fn rlp_append(&self, s: &mut RlpStream) {
		self.rlp_append(s, true);
	}
}

impl SpendDescription {
	fn rlp_append(&self, s: &mut RlpStream, include_sig: bool) {
		s.begin_list(6);
		let mut cv_vec = Vec::new();
		self.cv.write(&mut cv_vec);
		s.append(&cv_vec);
		let mut anchor_vec: Vec<u8> = Vec::new();
		self.anchor.into_repr().write_le(&mut anchor_vec);
		s.append(&anchor_vec);
		s.append(&&self.nullifier[..]);
		let mut rk_vec = Vec::new();
		((self.rk).0).write(&mut rk_vec);
		s.append(&rk_vec);
		s.append(&&self.zkproof[..]);
		if include_sig && self.spend_auth_sig.is_some() {
			s.append(self.spend_auth_sig.as_ref().unwrap());
		} else {
			s.append_empty_data();
		}
	}

	pub fn rlp_append_unsigned(&self, s: &mut RlpStream) {
		self.rlp_append(s, false);
    }
	pub fn read<R: Read>(mut reader: &mut R) -> io::Result<Self> {
		// Consensus rules (§4.4):
		// - Canonical encoding is enforced here.
		// - "Not small order" is enforced in SaplingVerificationContext::check_spend()
		//   (located in zcash_proofs::sapling::verifier).
		let cv = edwards::Point::<Bls12, Unknown>::read(&mut reader, &JUBJUB)?;

		// Consensus rule (§7.3): Canonical encoding is enforced here
		let anchor = {
			let mut f = FrRepr::default();
			f.read_le(&mut reader)?;
			Fr::from_repr(f).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
		};

		let mut nullifier = [0; 32];
		reader.read_exact(&mut nullifier)?;

		// Consensus rules (§4.4):
		// - Canonical encoding is enforced here.
		// - "Not small order" is enforced in SaplingVerificationContext::check_spend()
		let rk = PublicKey::<Bls12>::read(&mut reader, &JUBJUB)?;

		// Consensus rules (§4.4):
		// - Canonical encoding is enforced by the API of SaplingVerificationContext::check_spend()
		//   due to the need to parse this into a bellman::groth16::Proof.
		// - Proof validity is enforced in SaplingVerificationContext::check_spend()
		let mut zkproof = [0; GROTH_PROOF_SIZE];
		reader.read_exact(&mut zkproof)?;

		// Consensus rules (§4.4):
		// - Canonical encoding is enforced here.
		// - Signature validity is enforced in SaplingVerificationContext::check_spend()
		let spend_auth_sig = Some(Signature::read(&mut reader)?);

		Ok(SpendDescription {
			cv,
			anchor,
			nullifier,
			rk,
			zkproof,
			spend_auth_sig,
		})
	}

	pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
		self.cv.write(&mut writer)?;
		self.anchor.into_repr().write_le(&mut writer)?;
		writer.write_all(&self.nullifier)?;
		self.rk.write(&mut writer)?;
		writer.write_all(&self.zkproof)?;
		match self.spend_auth_sig {
			Some(sig) => sig.write(&mut writer),
			None =>
				return Err(io::Error::new(
					io::ErrorKind::InvalidInput,
					"Missing spend auth signature",
				)),
		}
	}
}

#[derive(Clone)]
pub struct OutputDescription {
	pub cv: edwards::Point<Bls12, Unknown>,
	pub cmu: Fr,
	pub ephemeral_key: edwards::Point<Bls12, Unknown>,
	pub enc_ciphertext: [u8; 580],
	pub out_ciphertext: [u8; 80],
	pub zkproof: [u8; GROTH_PROOF_SIZE],
}

impl std::fmt::Debug for OutputDescription {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
		write!(
			f,
			"OutputDescription(cv = {:?}, cmu = {:?}, ephemeral_key = {:?})",
			self.cv, self.cmu, self.ephemeral_key
		)
	}
}

impl PartialEq for OutputDescription {
	fn eq(&self, other: &OutputDescription) -> bool {
		self.cv == other.cv
			&& self.cmu == other.cmu
			&& self.ephemeral_key == other.ephemeral_key
			&& &self.enc_ciphertext[..] == &other.enc_ciphertext[..]
			&& &self.out_ciphertext[..] == &other.out_ciphertext[..]
			&& &self.zkproof[..] == &other.zkproof[..]
	}
}
impl Eq for OutputDescription {}

impl rlp::Decodable for OutputDescription {
	fn decode(d: &Rlp) -> Result<Self, DecoderError> {
		if d.item_count()? != 6 {
			return Err(DecoderError::RlpIncorrectListLen);
		}
		let vec: Vec<u8> = d.val_at(0)?;
		let cv = edwards::Point::<Bls12, Unknown>::read(&mut &vec[..], &JUBJUB);
		let cv = match cv {
			Ok(v) => v,
			Err(_) => return Err(DecoderError::Custom("Invaild Point Rlp")),
		};

		let mut fr_repr = FrRepr::default();
		let cmu_vec: Vec<u8> = d.val_at(1)?;
		if cmu_vec.len() != 32 {
			return Err(DecoderError::Custom("Invalid cmu Rlp."));
		}
		fr_repr.read_le(&mut &cmu_vec[..]);
		let cmu = match Fr::from_repr(fr_repr) {
			Ok(fr) => fr,
			Err(_) => return Err(DecoderError::Custom("Invalid prime field for cmu.")),
		};

		let vec: Vec<u8> = d.val_at(2)?;
		let ephemeral_key = edwards::Point::<Bls12, Unknown>::read(&mut &vec[..], &JUBJUB);
		let ephemeral_key = match ephemeral_key {
			Ok(v) => v,
			Err(_) => return Err(DecoderError::Custom("Invaild Ephemeral_Key Rlp")),
		};

		let mut enc_ciphertext: [u8; 580] = [0; 580];
		let enc_ciphertext_vec: Vec<u8> = d.val_at(3)?;
		if enc_ciphertext_vec.len() != enc_ciphertext.len() {
			return Err(DecoderError::Custom("Invalid Enc_Ciphertext Rlp."));
		}
		enc_ciphertext.copy_from_slice(&enc_ciphertext_vec);

		let mut out_ciphertext: [u8; 80] = [0; 80];
		let out_ciphertext_vec: Vec<u8> = d.val_at(4)?;
		if out_ciphertext_vec.len() != out_ciphertext.len() {
			return Err(DecoderError::Custom("Invalid Out_Ciphertext Rlp."));
		}
		out_ciphertext.copy_from_slice(&out_ciphertext_vec);

		let mut zkproof: [u8; GROTH_PROOF_SIZE] = [0; GROTH_PROOF_SIZE];
		let zkproof_vec: Vec<u8> = d.val_at(5)?;
		if zkproof_vec.len() != zkproof.len() {
			return Err(DecoderError::Custom("Invalid Zkproof Rlp."));
		}
		zkproof.copy_from_slice(&zkproof_vec);

		Ok(OutputDescription {
			cv,
			cmu,
			ephemeral_key,
			enc_ciphertext,
			out_ciphertext,
			zkproof,
		})
	}
}

impl rlp::Encodable for OutputDescription {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(6);
		let mut encoded = Vec::new();
		self.cv.write(&mut encoded);
		s.append(&encoded);
		let mut cmu_vec = Vec::new();
		self.cmu.into_repr().write_le(&mut cmu_vec);
		s.append(&cmu_vec);
		let mut encoded = Vec::new();
		self.ephemeral_key.write(&mut encoded);
		s.append(&encoded);
		s.append(&&self.enc_ciphertext[..]);

		s.append(&&self.out_ciphertext[..]);
		s.append(&&self.zkproof[..]);
	}
}

impl OutputDescription {
	pub fn read<R: Read>(mut reader: &mut R) -> io::Result<Self> {
		// Consensus rules (§4.5):
		// - Canonical encoding is enforced here.
		// - "Not small order" is enforced in SaplingVerificationContext::check_output()
		//   (located in zcash_proofs::sapling::verifier).
		let cv = edwards::Point::<Bls12, Unknown>::read(&mut reader, &JUBJUB)?;

		// Consensus rule (§7.4): Canonical encoding is enforced here
		let cmu = {
			let mut f = FrRepr::default();
			f.read_le(&mut reader)?;
			Fr::from_repr(f).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
		};

		// Consensus rules (§4.5):
		// - Canonical encoding is enforced here.
		// - "Not small order" is enforced in SaplingVerificationContext::check_output()
		let ephemeral_key = edwards::Point::<Bls12, Unknown>::read(&mut reader, &JUBJUB)?;

		let mut enc_ciphertext = [0; 580];
		let mut out_ciphertext = [0; 80];
		reader.read_exact(&mut enc_ciphertext)?;
		reader.read_exact(&mut out_ciphertext)?;

		// Consensus rules (§4.5):
		// - Canonical encoding is enforced by the API of SaplingVerificationContext::check_output()
		//   due to the need to parse this into a bellman::groth16::Proof.
		// - Proof validity is enforced in SaplingVerificationContext::check_output()
		let mut zkproof = [0; GROTH_PROOF_SIZE];
		reader.read_exact(&mut zkproof)?;

		Ok(OutputDescription {
			cv,
			cmu,
			ephemeral_key,
			enc_ciphertext,
			out_ciphertext,
			zkproof,
		})
	}

	pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
		self.cv.write(&mut writer)?;
		self.cmu.into_repr().write_le(&mut writer)?;
		self.ephemeral_key.write(&mut writer)?;
		writer.write_all(&self.enc_ciphertext)?;
		writer.write_all(&self.out_ciphertext)?;
		writer.write_all(&self.zkproof)
	}
}

enum SproutProof {
	Groth([u8; GROTH_PROOF_SIZE]),
	PHGR([u8; PHGR_PROOF_SIZE]),
}

impl std::fmt::Debug for SproutProof {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
		match self {
			SproutProof::Groth(_) => write!(f, "SproutProof::Groth"),
			SproutProof::PHGR(_) => write!(f, "SproutProof::PHGR"),
		}
	}
}

pub struct JSDescription {
	vpub_old: Amount,
	vpub_new: Amount,
	anchor: [u8; 32],
	nullifiers: [[u8; 32]; ZC_NUM_JS_INPUTS],
	commitments: [[u8; 32]; ZC_NUM_JS_OUTPUTS],
	ephemeral_key: [u8; 32],
	random_seed: [u8; 32],
	macs: [[u8; 32]; ZC_NUM_JS_INPUTS],
	proof: SproutProof,
	ciphertexts: [[u8; 601]; ZC_NUM_JS_OUTPUTS],
}

impl std::fmt::Debug for JSDescription {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
		write!(
			f,
			"JSDescription(
                vpub_old = {:?}, vpub_new = {:?},
                anchor = {:?},
                nullifiers = {:?},
                commitments = {:?},
                ephemeral_key = {:?},
                random_seed = {:?},
                macs = {:?})",
			self.vpub_old,
			self.vpub_new,
			self.anchor,
			self.nullifiers,
			self.commitments,
			self.ephemeral_key,
			self.random_seed,
			self.macs
		)
	}
}

impl JSDescription {
	pub fn read<R: Read>(mut reader: R, use_groth: bool) -> io::Result<Self> {
		// Consensus rule (§4.3): Canonical encoding is enforced here
		let vpub_old = Amount::read_u64(&mut reader)?;

		// Consensus rule (§4.3): Canonical encoding is enforced here
		let vpub_new = Amount::read_u64(&mut reader)?;

		// Consensus rule (§4.3): One of vpub_old and vpub_new being zero is
		// enforced by CheckTransactionWithoutProofVerification() in zcashd.

		let mut anchor = [0; 32];
		reader.read_exact(&mut anchor)?;

		let mut nullifiers = [[0; 32]; ZC_NUM_JS_INPUTS];
		nullifiers
			.iter_mut()
			.map(|nf| reader.read_exact(nf))
			.collect::<io::Result<()>>()?;

		let mut commitments = [[0; 32]; ZC_NUM_JS_OUTPUTS];
		commitments
			.iter_mut()
			.map(|cm| reader.read_exact(cm))
			.collect::<io::Result<()>>()?;

		// Consensus rule (§4.3): Canonical encoding is enforced by
		// ZCNoteDecryption::decrypt() in zcashd
		let mut ephemeral_key = [0; 32];
		reader.read_exact(&mut ephemeral_key)?;

		let mut random_seed = [0; 32];
		reader.read_exact(&mut random_seed)?;

		let mut macs = [[0; 32]; ZC_NUM_JS_INPUTS];
		macs.iter_mut()
			.map(|mac| reader.read_exact(mac))
			.collect::<io::Result<()>>()?;

		let proof = match use_groth {
			true => {
				// Consensus rules (§4.3):
				// - Canonical encoding is enforced in librustzcash_sprout_verify()
				// - Proof validity is enforced in librustzcash_sprout_verify()
				let mut proof = [0; GROTH_PROOF_SIZE];
				reader.read_exact(&mut proof)?;
				SproutProof::Groth(proof)
			}
			false => {
				// Consensus rules (§4.3):
				// - Canonical encoding is enforced by PHGRProof in zcashd
				// - Proof validity is enforced by JSDescription::Verify() in zcashd
				let mut proof = [0; PHGR_PROOF_SIZE];
				reader.read_exact(&mut proof)?;
				SproutProof::PHGR(proof)
			}
		};

		let mut ciphertexts = [[0; 601]; ZC_NUM_JS_OUTPUTS];
		ciphertexts
			.iter_mut()
			.map(|ct| reader.read_exact(ct))
			.collect::<io::Result<()>>()?;

		Ok(JSDescription {
			vpub_old,
			vpub_new,
			anchor,
			nullifiers,
			commitments,
			ephemeral_key,
			random_seed,
			macs,
			proof,
			ciphertexts,
		})
	}

	pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
		writer.write_i64::<LittleEndian>(self.vpub_old.0)?;
		writer.write_i64::<LittleEndian>(self.vpub_new.0)?;
		writer.write_all(&self.anchor)?;
		writer.write_all(&self.nullifiers[0])?;
		writer.write_all(&self.nullifiers[1])?;
		writer.write_all(&self.commitments[0])?;
		writer.write_all(&self.commitments[1])?;
		writer.write_all(&self.ephemeral_key)?;
		writer.write_all(&self.random_seed)?;
		writer.write_all(&self.macs[0])?;
		writer.write_all(&self.macs[1])?;

		match &self.proof {
			SproutProof::Groth(p) => writer.write_all(p)?,
			SproutProof::PHGR(p) => writer.write_all(p)?,
		}

		writer.write_all(&self.ciphertexts[0])?;
		writer.write_all(&self.ciphertexts[1])
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sapling_crypto::jubjub::JubjubBls12;

	#[test]
	fn amount_in_range() {
		let zero = b"\x00\x00\x00\x00\x00\x00\x00\x00";
		assert_eq!(Amount::read_u64(&zero[..]).unwrap(), Amount(0));
		assert_eq!(Amount::read_i64(&zero[..], false).unwrap(), Amount(0));
		assert_eq!(Amount::read_i64(&zero[..], true).unwrap(), Amount(0));

		let neg_one = b"\xff\xff\xff\xff\xff\xff\xff\xff";
		assert!(Amount::read_u64(&neg_one[..]).is_err());
		assert!(Amount::read_i64(&neg_one[..], false).is_err());
		assert_eq!(Amount::read_i64(&neg_one[..], true).unwrap(), Amount(-1));

		let max_money = b"\x00\x40\x07\x5a\xf0\x75\x07\x00";
		assert_eq!(Amount::read_u64(&max_money[..]).unwrap(), Amount(MAX_MONEY));
		assert_eq!(
			Amount::read_i64(&max_money[..], false).unwrap(),
			Amount(MAX_MONEY)
		);
		assert_eq!(
			Amount::read_i64(&max_money[..], true).unwrap(),
			Amount(MAX_MONEY)
		);

		let max_money_p1 = b"\x01\x40\x07\x5a\xf0\x75\x07\x00";
		assert!(Amount::read_u64(&max_money_p1[..]).is_err());
		assert!(Amount::read_i64(&max_money_p1[..], false).is_err());
		assert!(Amount::read_i64(&max_money_p1[..], true).is_err());

		let neg_max_money = b"\x00\xc0\xf8\xa5\x0f\x8a\xf8\xff";
		assert!(Amount::read_u64(&neg_max_money[..]).is_err());
		assert!(Amount::read_i64(&neg_max_money[..], false).is_err());
		assert_eq!(
			Amount::read_i64(&neg_max_money[..], true).unwrap(),
			Amount(-MAX_MONEY)
		);

		let neg_max_money_m1 = b"\xff\xbf\xf8\xa5\x0f\x8a\xf8\xff";
		assert!(Amount::read_u64(&neg_max_money_m1[..]).is_err());
		assert!(Amount::read_i64(&neg_max_money_m1[..], false).is_err());
		assert!(Amount::read_i64(&neg_max_money_m1[..], true).is_err());
	}

	#[test]
	fn encode_decode_spend_description() {
		// Create a dummy spend for testing.
		let spend = SpendDescription {
			cv: edwards::Point::zero(),
			anchor: Fr::from_repr(FrRepr::from(1)).unwrap(),
			nullifier: [0u8; 32],
			rk: PublicKey::read(&[0u8; 64][..], &JubjubBls12::new()).unwrap(),
			zkproof: [1u8; GROTH_PROOF_SIZE],
			spend_auth_sig: Some(Signature::read(&[0u8; 64][..]).unwrap()),
		};
		let encoded = ::rlp::encode(&spend);
		let spend2: SpendDescription = rlp::decode(&encoded).unwrap();
		assert_eq!(spend, spend2);
	}

	#[test]
	fn encode_decode_output_description() {
		let output = OutputDescription {
			cv: edwards::Point::zero(),
			cmu: Fr::from_repr(FrRepr::from(1)).unwrap(),
			ephemeral_key: edwards::Point::zero(),
			enc_ciphertext: [0u8; 580],
			out_ciphertext: [0u8; 80],
			zkproof: [0u8; GROTH_PROOF_SIZE],
		};
		let encoded = ::rlp::encode(&output);
		let output2: OutputDescription = rlp::decode(&encoded).unwrap();
		assert_eq!(output, output2);
	}
}
