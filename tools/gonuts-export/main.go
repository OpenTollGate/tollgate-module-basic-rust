package main

import (
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"sort"

	bolt "go.etcd.io/bbolt"
)

// Bucket names — must match gonuts wallet/storage/bolt.go
const (
	KEYSETS_BUCKET = "keysets"
	PROOFS_BUCKET  = "proofs"
	SEED_BUCKET    = "seed"
)

// Proof mirrors the gonuts cashu.Proof JSON structure.
// We re-declare it here to avoid importing the gonuts package
// (which would pull in secp256k1, hdkeychain, etc.).
type Proof struct {
	Amount  uint64  `json:"amount"`
	Id      string  `json:"id"`
	Secret  string  `json:"secret"`
	C       string  `json:"C"`
	Witness string  `json:"witness,omitempty"`
	DLEQ    *DLEQ   `json:"dleq,omitempty"`
}

type DLEQ struct {
	E string `json:"e"`
	S string `json:"s"`
	R string `json:"r,omitempty"`
}

// WalletKeyset mirrors gonuts crypto.WalletKeyset JSON structure.
// The marshalled form uses []byte for public keys, not the struct form.
type WalletKeyset struct {
	Id          string             `json:"Id"`
	MintURL     string             `json:"MintURL"`
	Unit        string             `json:"Unit"`
	Active      bool               `json:"Active"`
	PublicKeys  map[uint64][]byte   `json:"PublicKeys"`
	Counter     uint32             `json:"Counter"`
	InputFeePpk uint               `json:"InputFeePpk"`
}

// KeysetsMap maps mint URL → list of keysets.
type KeysetsMap map[string][]WalletKeyset

// TokenV3 is the Cashu V3 token format (NUT-00).
type TokenV3 struct {
	Token []TokenV3Proof `json:"token"`
	Unit  string         `json:"unit"`
	Memo  string         `json:"memo,omitempty"`
}

type TokenV3Proof struct {
	Mint   string  `json:"mint"`
	Proofs []Proof `json:"proofs"`
}

// KeysetCounterEntry for keyset_counters.json
type KeysetCounterEntry struct {
	KeysetID string `json:"keyset_id"`
	MintURL  string `json:"mint_url"`
	Counter  uint32 `json:"counter"`
}

// KeysetHealth for migration-report.json
type KeysetHealth struct {
	KeysetID       string `json:"keyset_id"`
	MintURL        string `json:"mint_url"`
	Counter        uint32 `json:"counter"`
	ProofCount     int    `json:"proof_count"`
	TotalAmount    uint64 `json:"total_amount"`
	Status         string `json:"status"`
}

type MigrationReport struct {
	ExportedAt      string         `json:"exported_at"`
	Keysets         []KeysetHealth `json:"keysets"`
	TotalProofs     int            `json:"total_proofs"`
	TotalAmount     uint64         `json:"total_amount"`
	HasSeed         bool           `json:"has_seed"`
}

func main() {
	boltPath := flag.String("bolt", "", "path to gonuts wallet.db (required)")
	outDir := flag.String("out", ".", "output directory for tokens.jsonl, keyset_counters.json, migration-report.json")
	flag.Parse()

	if *boltPath == "" {
		fmt.Fprintln(os.Stderr, "error: --bolt is required")
		flag.Usage()
		os.Exit(1)
	}

	if err := run(*boltPath, *outDir); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func run(boltPath, outDir string) error {
	// Open bbolt read-only
	db, err := bolt.Open(boltPath, 0600, &bolt.Options{ReadOnly: true})
	if err != nil {
		return fmt.Errorf("opening bbolt: %w", err)
	}
	defer db.Close()

	// Load keysets
	keysets, err := loadKeysets(db)
	if err != nil {
		return fmt.Errorf("loading keysets: %w", err)
	}

	// Load proofs
	proofsBykeyset, err := loadProofs(db)
	if err != nil {
		return fmt.Errorf("loading proofs: %w", err)
	}

	// Check for seed
	hasSeed := false
	db.View(func(tx *bolt.Tx) error {
		seedb := tx.Bucket([]byte(SEED_BUCKET))
		if seedb != nil {
			seed := seedb.Get([]byte(SEED_BUCKET))
			hasSeed = len(seed) > 0
		}
		return nil
	})

	// Build output
	if err := os.MkdirAll(outDir, 0755); err != nil {
		return fmt.Errorf("creating output dir: %w", err)
	}

	// Emit tokens.jsonl — one V3 token per mint+keyset group
	tokensPath := filepath.Join(outDir, "tokens.jsonl")
	tokensFile, err := os.Create(tokensPath)
	if err != nil {
		return fmt.Errorf("creating tokens.jsonl: %w", err)
	}
	defer tokensFile.Close()

	totalProofs := 0
	totalAmount := uint64(0)
	var healthEntries []KeysetHealth

	for mintURL, ksList := range keysets {
		for _, ks := range ksList {
			proofs := proofsBykeyset[ks.Id]
			if len(proofs) == 0 {
				healthEntries = append(healthEntries, KeysetHealth{
					KeysetID:   ks.Id,
					MintURL:    mintURL,
					Counter:    ks.Counter,
					ProofCount: 0,
					Status:     "empty",
				})
				continue
			}

			// Sort proofs by amount for deterministic output
			sort.Slice(proofs, func(i, j int) bool {
				return proofs[i].Amount < proofs[j].Amount
			})

			// Create V3 token
			token := TokenV3{
				Token: []TokenV3Proof{{
					Mint:   mintURL,
					Proofs: proofs,
				}},
				Unit: "sat",
			}

			tokenJSON, err := json.Marshal(token)
			if err != nil {
				return fmt.Errorf("marshalling token for keyset %s: %w", ks.Id, err)
			}

			// Write one token per line (JSONL)
			tokensFile.Write(append(tokenJSON, '\n'))

			// Calculate health
			ksAmount := uint64(0)
			for _, p := range proofs {
				ksAmount += p.Amount
			}

			healthEntries = append(healthEntries, KeysetHealth{
				KeysetID:    ks.Id,
				MintURL:     mintURL,
				Counter:     ks.Counter,
				ProofCount:  len(proofs),
				TotalAmount: ksAmount,
				Status:      "healthy",
			})

			totalProofs += len(proofs)
			totalAmount += ksAmount
		}
	}

	// Emit keyset_counters.json
	countersPath := filepath.Join(outDir, "keyset_counters.json")
	var counters []KeysetCounterEntry
	for _, h := range healthEntries {
		counters = append(counters, KeysetCounterEntry{
			KeysetID: h.KeysetID,
			MintURL:  h.MintURL,
			Counter:  h.Counter,
		})
	}
	countersJSON, err := json.MarshalIndent(counters, "", "  ")
	if err != nil {
		return fmt.Errorf("marshalling counters: %w", err)
	}
	if err := os.WriteFile(countersPath, countersJSON, 0644); err != nil {
		return fmt.Errorf("writing keyset_counters.json: %w", err)
	}

	// Emit migration-report.json
	report := MigrationReport{
		ExportedAt:  "", // caller can set
		Keysets:     healthEntries,
		TotalProofs: totalProofs,
		TotalAmount: totalAmount,
		HasSeed:     hasSeed,
	}
	reportJSON, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		return fmt.Errorf("marshalling report: %w", err)
	}
	reportPath := filepath.Join(outDir, "migration-report.json")
	if err := os.WriteFile(reportPath, reportJSON, 0644); err != nil {
		return fmt.Errorf("writing migration-report.json: %w", err)
	}

	fmt.Printf("Export complete: %d proofs, %d sats across %d keysets\n",
		totalProofs, totalAmount, len(healthEntries))
	fmt.Printf("  tokens.jsonl → %s\n", tokensPath)
	fmt.Printf("  keyset_counters.json → %s\n", countersPath)
	fmt.Printf("  migration-report.json → %s\n", reportPath)

	return nil
}

// loadKeysets reads the nested keysets bucket structure.
func loadKeysets(db *bolt.DB) (KeysetsMap, error) {
	keysets := make(KeysetsMap)

	err := db.View(func(tx *bolt.Tx) error {
		keysetsb := tx.Bucket([]byte(KEYSETS_BUCKET))
		if keysetsb == nil {
			return nil // no keysets bucket
		}

		return keysetsb.ForEach(func(mintURL, _ []byte) error {
			mintBucket := keysetsb.Bucket(mintURL)
			if mintBucket == nil {
				return nil
			}

			var mintKeysets []WalletKeyset
			c := mintBucket.Cursor()
			for k, v := c.First(); k != nil; k, v = c.Next() {
				var ks WalletKeyset
				if err := json.Unmarshal(v, &ks); err != nil {
					return fmt.Errorf("unmarshalling keyset %s: %w", string(k), err)
				}
				mintKeysets = append(mintKeysets, ks)
			}
			keysets[string(mintURL)] = mintKeysets
			return nil
		})
	})

	return keysets, err
}

// loadProofs reads the proofs bucket and groups by keyset ID.
func loadProofs(db *bolt.DB) (map[string][]Proof, error) {
	byKeyset := make(map[string][]Proof)

	err := db.View(func(tx *bolt.Tx) error {
		proofsb := tx.Bucket([]byte(PROOFS_BUCKET))
		if proofsb == nil {
			return nil // no proofs bucket
		}

		c := proofsb.Cursor()
		for k, v := c.First(); k != nil; k, v = c.Next() {
			var p Proof
			if err := json.Unmarshal(v, &p); err != nil {
				// Skip malformed proofs (matches gonuts behavior)
				continue
			}
			byKeyset[p.Id] = append(byKeyset[p.Id], p)
		}
		return nil
	})

	return byKeyset, err
}

// helper: hex-decode a string, returning empty slice on error.
func mustHex(s string) []byte {
	b, err := hex.DecodeString(s)
	if err != nil {
		return nil
	}
	return b
}