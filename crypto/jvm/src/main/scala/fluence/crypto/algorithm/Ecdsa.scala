/*
 * Copyright (C) 2017  Fluence Labs Limited
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of the
 * License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 */

package fluence.crypto.algorithm

import java.math.BigInteger
import java.security._
import java.security.interfaces.ECPrivateKey

import cats.data.EitherT
import cats.{ Applicative, Monad, MonadError }
import cats.syntax.flatMap._
import cats.syntax.functor._
import fluence.crypto.keypair.KeyPair
import org.bouncycastle.jce.ECNamedCurveTable
import org.bouncycastle.jce.interfaces.ECPublicKey
import org.bouncycastle.jce.provider.BouncyCastleProvider
import org.bouncycastle.jce.spec.{ ECNamedCurveParameterSpec, ECParameterSpec, ECPrivateKeySpec, ECPublicKeySpec }
import scodec.bits.ByteVector

import scala.language.higherKinds
import scala.util.control.NonFatal

/**
 * Elliptic Curve Digital Signature Algorithm
 * @param curveType http://www.bouncycastle.org/wiki/display/JA1/Supported+Curves+%28ECDSA+and+ECGOST%29
 * @param scheme https://bouncycastle.org/specifications.html
 */
class Ecdsa(curveType: String, scheme: String) extends JavaAlgorithm
  with SignatureFunctions with KeyGenerator {
  import Ecdsa._

  private def nonFatalHandling[F[_] : Applicative, A](a: ⇒ A)(errorText: String): EitherT[F, CryptoErr, A] = {
    try EitherT.pure(a)
    catch {
      case NonFatal(e) ⇒ EitherT.leftT(CryptoErr(errorText + " " + e.getLocalizedMessage))
    }
  }

  override def generateKeyPair[F[_] : Monad](random: SecureRandom): EitherT[F, CryptoErr, KeyPair] =
    for {
      ecSpec ← EitherT.fromOption[F](
        Option(ECNamedCurveTable.getParameterSpec(curveType)),
        CryptoErr("Parameter spec for the curve is not available."))
      g ← getKeyPairGenerator
      _ ← nonFatalHandling(g.initialize(ecSpec, random))("Could not initialize KeyPairGenerator.")
      p ← EitherT.fromOption(
        Option(g.generateKeyPair()),
        CryptoErr("Could not generate KeyPair. Unexpected."))

      keyPair ← nonFatalHandling {
        //store S number for private key and compressed Q point on curve for public key
        val pk = p.getPublic.asInstanceOf[ECPublicKey].getQ.getEncoded(true)
        val sk = p.getPrivate.asInstanceOf[ECPrivateKey].getS.toByteArray
        KeyPair.fromBytes(pk, sk)
      }("Can't get public/private key from generated keypair")
    } yield keyPair

  override def generateKeyPair[F[_] : Monad](): EitherT[F, CryptoErr, KeyPair] =
    generateKeyPair(new SecureRandom())

  override def sign[F[_] : Monad](keyPair: KeyPair, message: ByteVector): EitherT[F, CryptoErr, fluence.crypto.signature.Signature] =
    signMessage(keyPair.secretKey.value.toArray, message.toArray)
      .map(bb ⇒ fluence.crypto.signature.Signature(keyPair.publicKey, ByteVector(bb)))

  override def verify[F[_] : Monad](signature: fluence.crypto.signature.Signature, message: ByteVector): EitherT[F, CryptoErr, Unit] = {
    val publicKey = signature.publicKey.value.toArray
    val messageBytes = message.toArray
    val signatureBytes = signature.sign.toArray

    for {
      ec ← curveSpec
      keySpec ← nonFatalHandling(new ECPublicKeySpec(ec.getCurve.decodePoint(publicKey), ec))("Cannot read public key.")
      keyFactory ← getKeyFactory
      signProvider ← getSignatureProvider
      verify ← {
        nonFatalHandling {
          signProvider.initVerify(keyFactory.generatePublic(keySpec))
          signProvider.update(messageBytes)
          signProvider.verify(signatureBytes)
        }("Cannot verify message.")
      }
      _ ← EitherT.cond[F](verify, (), CryptoErr("Signature is not verified"))
    } yield ()
  }

  private def signMessage[F[_] : Monad](privateKey: Array[Byte], message: Array[Byte]): EitherT[F, CryptoErr, Array[Byte]] = {
    for {
      ec ← curveSpec
      keySpec ← nonFatalHandling(new ECPrivateKeySpec(new BigInteger(privateKey), ec))("Cannot read private key.")
      keyFactory ← getKeyFactory
      signProvider ← getSignatureProvider
      sign ← {
        nonFatalHandling {
          signProvider.initSign(keyFactory.generatePrivate(keySpec))
          signProvider.update(message)
          signProvider.sign()
        }("Cannot sign message.")
      }
    } yield sign
  }

  private def curveSpec[F[_] : Monad] =
    nonFatalHandling(ECNamedCurveTable.getParameterSpec(curveType).asInstanceOf[ECParameterSpec])("Cannot get curve parameters.")

  private def getKeyPairGenerator[F[_] : Monad] =
    nonFatalHandling(KeyPairGenerator.getInstance(ECDSA, BouncyCastleProvider.PROVIDER_NAME))("Cannot get key pair generator.")

  private def getKeyFactory[F[_] : Monad] =
    nonFatalHandling(KeyFactory.getInstance(ECDSA, BouncyCastleProvider.PROVIDER_NAME))("Cannot get key factory instance.")

  private def getSignatureProvider[F[_] : Monad] =
    nonFatalHandling(Signature.getInstance(scheme, BouncyCastleProvider.PROVIDER_NAME))("Cannot get signature instance.")
}

object Ecdsa {
  //algorithm name in security provider
  val ECDSA = "ECDSA"

  /**
   * size of key is 256 bit
   * `secp256k1` refers to the parameters of the ECDSA curve
   * `SHA256withECDSA` Preferably the size of the key is greater than or equal to the digest algorithm
   */
  lazy val ecdsa_secp256k1_sha256 = new Ecdsa("secp256k1", "SHA256withECDSA")
}