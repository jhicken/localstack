// lstack-sign makes and uses the ed25519 key that signs the Tailscale helper's release manifest. lstack embeds
// the public key and refuses any helper whose manifest isn't signed by it.
//
//	lstack-sign keygen          print a new private key (base64 seed — keep it secret) and its public key (hex)
//	lstack-sign pub             print the public key for TAILNET_SIGNING_KEY
//	lstack-sign sign <file>     print the hex signature of <file> with TAILNET_SIGNING_KEY
//	lstack-sign verify <file> <sig-file> <public-hex>
package main

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/hex"
	"fmt"
	"os"
	"strings"
)

func main() {
	if len(os.Args) < 2 {
		usage()
	}
	switch os.Args[1] {
	case "keygen":
		pub, priv, err := ed25519.GenerateKey(rand.Reader)
		check(err)
		fmt.Println("private (TAILNET_SIGNING_KEY):", base64.StdEncoding.EncodeToString(priv.Seed()))
		fmt.Println("public  (TAILNET_PUBLIC_KEY): ", hex.EncodeToString(pub))
	case "pub":
		fmt.Println(hex.EncodeToString(key().Public().(ed25519.PublicKey)))
	case "sign":
		if len(os.Args) != 3 {
			usage()
		}
		msg, err := os.ReadFile(os.Args[2])
		check(err)
		fmt.Println(hex.EncodeToString(ed25519.Sign(key(), msg)))
	case "verify":
		if len(os.Args) != 5 {
			usage()
		}
		msg, err := os.ReadFile(os.Args[2])
		check(err)
		sigHex, err := os.ReadFile(os.Args[3])
		check(err)
		sig, err := hex.DecodeString(strings.TrimSpace(string(sigHex)))
		check(err)
		pub, err := hex.DecodeString(os.Args[4])
		check(err)
		if len(pub) != ed25519.PublicKeySize || !ed25519.Verify(pub, msg, sig) {
			fmt.Fprintln(os.Stderr, "✗ bad signature")
			os.Exit(1)
		}
		fmt.Println("✓ signature ok")
	default:
		usage()
	}
}

func key() ed25519.PrivateKey {
	seed, err := base64.StdEncoding.DecodeString(strings.TrimSpace(os.Getenv("TAILNET_SIGNING_KEY")))
	if err != nil || len(seed) != ed25519.SeedSize {
		fmt.Fprintln(os.Stderr, "TAILNET_SIGNING_KEY must be a base64 32-byte seed (make one with `lstack-sign keygen`)")
		os.Exit(2)
	}
	return ed25519.NewKeyFromSeed(seed)
}

func check(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprintln(os.Stderr, "usage: lstack-sign keygen | pub | sign <file> | verify <file> <sig-file> <public-hex>")
	os.Exit(2)
}
