package main

import (
	"fmt"
	"os"
)

func main() {
	vars := []string{"PRIVATE_KEY", "BEARER_TOKEN", "SUBACCOUNT_ADDRESS", "DRY_RUN", "APTOS_FULLNODE_URL"}
	for _, v := range vars {
		val := os.Getenv(v)
		if val == "" {
			fmt.Printf("%s=UNSET\n", v)
		} else {
			if v == "DRY_RUN" {
				fmt.Printf("%s=%s\n", v, val)
			} else {
				fmt.Printf("%s=SET\n", v)
			}
		}
	}
}
