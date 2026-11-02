require!(!vault.reentrancy_locked, ErrorCode::ReentrancyDetected);
   vault.reentrancy_locked = true;
   // ... external CPI invocations ...
   vault.reentrancy_locked = false;