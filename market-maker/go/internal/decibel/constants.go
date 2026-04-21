package decibel

import "strings"

// PackageAddrForNetwork returns the Decibel contract package address for the given network.
func PackageAddrForNetwork(network string) string {
	switch strings.ToLower(strings.TrimSpace(network)) {
	case "mainnet":
		return MainnetPackageAddr
	default:
		return TestnetPackageAddr
	}
}

// Package addresses
const (
	TestnetPackageAddr = "0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f"
	MainnetPackageAddr = "0x50ead22afd6ffd9769e3b3d6e0e64a2a350d68e8b102c4e72e33d0b8cfdfdb06"
)

// Entry function names
const (
	ModuleDexAccountsEntry = "dex_accounts_entry"

	FuncPlaceOrderToSubaccount     = "place_order_to_subaccount"
	FuncCancelOrder                = "cancel_order"
	FuncCancelAllOrders            = "cancel_all_orders"
	FuncPlaceBulkOrders            = "place_bulk_orders_to_subaccount"
	FuncDepositToSubaccount        = "deposit_to_subaccount"
	FuncWithdrawFromSubaccount     = "withdraw_from_subaccount"
	FuncCreateSubaccount           = "create_new_subaccount"
	FuncConfigureUserSettings      = "configure_user_settings_for_market"
)

// TimeInForce values
type TimeInForce uint8

const (
	TIFGoodTillCanceled  TimeInForce = 0
	TIFPostOnly          TimeInForce = 1
	TIFImmediateOrCancel TimeInForce = 2
)

// Network environments
const (
	EnvTestnet = "testnet"
	EnvMainnet = "mainnet"
)

// DecimalPlaces for price and size on Decibel
const (
	PriceDecimals = 9
	SizeDecimals  = 9
)
