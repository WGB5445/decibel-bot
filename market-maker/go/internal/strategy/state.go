package strategy

// String returns the human-readable state name
func (s StrategyState) String() string {
	switch s {
	case StateInit:
		return "INIT"
	case StateNoPosition:
		return "NO_POSITION"
	case StateMaking:
		return "MAKING"
	case StatePositionManage:
		return "POSITION_MANAGE"
	case StateCooldown:
		return "COOLDOWN"
	default:
		return "UNKNOWN"
	}
}
