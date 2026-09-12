# Runs the child with ARGS (a semicolon-separated list) and asserts its exit code.
# CTest's WILL_FAIL only distinguishes zero from non-zero; the child documents distinct
# codes (1 = usage error, 2 = the log file could not be written), so they are checked exactly.
# The separators arrive escaped, because `-DARGS=` has to survive CMake's own list handling.
string(REPLACE "\;" ";" ARGS "${ARGS}")
execute_process(COMMAND "${CHILD}" ${ARGS} RESULT_VARIABLE code)
if(NOT code EQUAL "${EXPECTED}")
  message(FATAL_ERROR "${CHILD} ${ARGS}: expected exit ${EXPECTED}, got ${code}")
endif()
