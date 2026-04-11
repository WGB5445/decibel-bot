package api

import (
	"encoding/json"
	"testing"
)

func TestUnmarshalAccountPositions_extended(t *testing.T) {
	const raw = `[
	  {
	    "market": "0xabc",
	    "user": "0xuser",
	    "size": -12.5,
	    "user_leverage": 10,
	    "entry_price": 0.00123,
	    "is_isolated": true,
	    "is_deleted": false,
	    "unrealized_funding": -0.42,
	    "estimated_liquidation_price": 0.0009,
	    "transaction_version": 9001,
	    "tp_order_id": "1",
	    "tp_trigger_price": 0.002,
	    "tp_limit_price": 0.0021,
	    "sl_order_id": null,
	    "sl_trigger_price": null,
	    "sl_limit_price": null,
	    "has_fixed_sized_tpsls": false
	  }
	]`
	var got []AccountPosition
	if err := json.Unmarshal([]byte(raw), &got); err != nil {
		t.Fatal(err)
	}
	if len(got) != 1 {
		t.Fatalf("len=%d", len(got))
	}
	p := got[0]
	if p.Market != "0xabc" || p.User != "0xuser" {
		t.Fatalf("ids: %+v", p)
	}
	if p.Size != -12.5 || p.UserLeverage != 10 || p.EntryPrice != 0.00123 {
		t.Fatalf("nums: %+v", p)
	}
	if !p.IsIsolated || p.IsDeleted {
		t.Fatalf("flags: %+v", p)
	}
	if p.UnrealizedFunding != -0.42 || p.EstimatedLiquidationPrice != 0.0009 {
		t.Fatalf("funding/liq: %+v", p)
	}
	if p.TransactionVersion != 9001 {
		t.Fatalf("tx ver: %d", p.TransactionVersion)
	}
	if p.TPOrderID == nil || *p.TPOrderID != "1" {
		t.Fatalf("tp id: %+v", p.TPOrderID)
	}
	if p.TPTriggerPrice == nil || *p.TPTriggerPrice != 0.002 {
		t.Fatalf("tp trig: %+v", p.TPTriggerPrice)
	}
	if p.SLOrderID != nil {
		t.Fatalf("sl id should be nil: %+v", p.SLOrderID)
	}
}

func TestUnmarshalAccountPositions_userFixture(t *testing.T) {
	const raw = `[{"market":"0x161b7b3f58327d057ee5824de0c1a4fc4fa3d121b847c138e921a255768a0dca","user":"0x7dc8a464e9998e9f34aea1c1fa0094f2c61c0393206ff3fbefc9387f9104b577","size":0.0006,"user_leverage":1,"entry_price":72657.0,"is_isolated":false,"is_deleted":false,"unrealized_funding":0.0,"estimated_liquidation_price":0.0,"transaction_version":8381508054,"tp_order_id":null,"tp_trigger_price":null,"tp_limit_price":null,"sl_order_id":null,"sl_trigger_price":null,"sl_limit_price":null,"has_fixed_sized_tpsls":false}]`
	var got []AccountPosition
	if err := json.Unmarshal([]byte(raw), &got); err != nil {
		t.Fatal(err)
	}
	if len(got) != 1 {
		t.Fatalf("len=%d want 1", len(got))
	}
	p := got[0]
	if p.Market == "" || p.User == "" {
		t.Fatalf("missing ids: %+v", p)
	}
	if p.Size != 0.0006 || p.UserLeverage != 1 || p.EntryPrice != 72657 {
		t.Fatalf("unexpected position values: %+v", p)
	}
	if p.IsIsolated || p.IsDeleted || p.HasFixedSizedTpsls {
		t.Fatalf("unexpected flags: %+v", p)
	}
	if p.UnrealizedFunding != 0 || p.EstimatedLiquidationPrice != 0 {
		t.Fatalf("unexpected funding/liq: %+v", p)
	}
	if p.TransactionVersion != 8381508054 {
		t.Fatalf("tx version: %d", p.TransactionVersion)
	}
	if p.TPOrderID != nil || p.SLOrderID != nil {
		t.Fatalf("expected nil tpsl ids: %+v", p)
	}
}

func TestUnmarshalMarketConfig_extended(t *testing.T) {
	const raw = `{
	  "market_addr": "0xdead",
	  "market_name": "kPEPE/USD",
	  "tick_size": 1,
	  "lot_size": 1000,
	  "min_size": 1000,
	  "px_decimals": 6,
	  "sz_decimals": 0,
	  "max_leverage": 50,
	  "mode": "linear",
	  "max_open_interest": 1e12,
	  "unrealized_pnl_haircut_bps": 5
	}`
	var m MarketConfig
	if err := json.Unmarshal([]byte(raw), &m); err != nil {
		t.Fatal(err)
	}
	if m.MarketAddr != "0xdead" || m.MarketName != "kPEPE/USD" {
		t.Fatalf("names: %+v", m)
	}
	if m.MaxLeverage != 50 || m.Mode != "linear" {
		t.Fatalf("meta: %+v", m)
	}
	if m.UnrealizedPnlHaircutBps != 5 {
		t.Fatalf("haircut: %d", m.UnrealizedPnlHaircutBps)
	}
}
