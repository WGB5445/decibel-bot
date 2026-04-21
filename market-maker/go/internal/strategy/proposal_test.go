package strategy

import (
	"testing"

	"github.com/shopspring/decimal"
)

func TestCreateBaseProposal(t *testing.T) {
	pb := NewProposalBuilder(
		decimal.NewFromFloat(0.01), // bid spread 1%
		decimal.NewFromFloat(0.01), // ask spread 1%
		decimal.NewFromFloat(0.1),  // order amount
		2,                          // 2 levels
		decimal.NewFromFloat(0.005), // level spread 0.5%
		decimal.NewFromFloat(0.05),  // level amount +0.05
		decimal.NewFromInt(-1),      // no ceiling
		decimal.NewFromInt(-1),      // no floor
		decimal.NewFromInt(-100),    // min spread disabled
		9, 9,
	)

	refPrice := decimal.NewFromFloat(10000)
	proposal := pb.CreateBaseProposal(refPrice)

	if len(proposal.Buys) != 2 {
		t.Fatalf("expected 2 buys, got %d", len(proposal.Buys))
	}
	if len(proposal.Sells) != 2 {
		t.Fatalf("expected 2 sells, got %d", len(proposal.Sells))
	}

	// Level 0 buy should be 1% below ref price => 9900
	buy0 := proposal.Buys[0].Price
	expectedBuy0 := decimal.NewFromFloat(9900)
	if !buy0.Equal(expectedBuy0) {
		t.Fatalf("buy0 price = %s, want %s", buy0.String(), expectedBuy0.String())
	}

	// Level 0 sell should be 1% above ref price => 10100
	sell0 := proposal.Sells[0].Price
	expectedSell0 := decimal.NewFromFloat(10100)
	if !sell0.Equal(expectedSell0) {
		t.Fatalf("sell0 price = %s, want %s", sell0.String(), expectedSell0.String())
	}

	// Level 0 size should be 0.1
	if !proposal.Buys[0].Size.Equal(decimal.NewFromFloat(0.1)) {
		t.Fatalf("buy0 size = %s, want 0.1", proposal.Buys[0].Size.String())
	}

	// Level 1 size should be 0.15
	if !proposal.Buys[1].Size.Equal(decimal.NewFromFloat(0.15)) {
		t.Fatalf("buy1 size = %s, want 0.15", proposal.Buys[1].Size.String())
	}
}

func TestApplyPriceBand(t *testing.T) {
	pb := NewProposalBuilder(
		decimal.NewFromFloat(0.01),
		decimal.NewFromFloat(0.01),
		decimal.NewFromFloat(0.1),
		1,
		decimal.Zero,
		decimal.Zero,
		decimal.NewFromFloat(110), // ceiling
		decimal.NewFromFloat(90),  // floor
		decimal.NewFromInt(-100),
		9, 9,
	)

	refPrice := decimal.NewFromFloat(115)
	proposal := pb.CreateBaseProposal(refPrice)
	pb.ApplyPriceBand(refPrice, proposal)

	if len(proposal.Buys) != 0 {
		t.Fatalf("expected 0 buys above ceiling, got %d", len(proposal.Buys))
	}
	if len(proposal.Sells) != 1 {
		t.Fatalf("expected 1 sell, got %d", len(proposal.Sells))
	}
}

func TestFilterOutTakers(t *testing.T) {
	pb := NewProposalBuilder(
		decimal.NewFromFloat(0.001),
		decimal.NewFromFloat(0.001),
		decimal.NewFromFloat(0.1),
		1,
		decimal.Zero,
		decimal.Zero,
		decimal.NewFromInt(-1),
		decimal.NewFromInt(-1),
		decimal.NewFromInt(-100),
		9, 9,
	)

	refPrice := decimal.NewFromFloat(10000)
	proposal := pb.CreateBaseProposal(refPrice)
	// best bid=9995, best ask=10005
	pb.FilterOutTakers(decimal.NewFromFloat(9995), decimal.NewFromFloat(10005), proposal)

	// buy price should be 9990 (1bp below 10000), which is < 10005 => kept
	if len(proposal.Buys) != 1 {
		t.Fatalf("expected 1 buy, got %d", len(proposal.Buys))
	}
	// sell price should be 10010, which is > 10005 => kept
	if len(proposal.Sells) != 1 {
		t.Fatalf("expected 1 sell, got %d", len(proposal.Sells))
	}

	// Now make buy cross: best ask = 9980
	proposal2 := pb.CreateBaseProposal(refPrice)
	pb.FilterOutTakers(decimal.NewFromFloat(9995), decimal.NewFromFloat(9980), proposal2)
	// buy price 9990 >= best ask 9980 => removed
	if len(proposal2.Buys) != 0 {
		t.Fatalf("expected 0 buys after taker filter, got %d", len(proposal2.Buys))
	}
}
