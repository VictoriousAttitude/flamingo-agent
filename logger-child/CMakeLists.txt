cmake_minimum_required(VERSION 3.15)
project(logger_child LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# Static MSVC runtime so the binary runs on a clean machine without the VC++ redistributable.
if(MSVC)
  set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
endif()

add_executable(logger-child src/main.cpp)
target_compile_options(logger-child PRIVATE
  $<$<CXX_COMPILER_ID:MSVC>:/W4 /permissive->
  $<$<NOT:$<CXX_COMPILER_ID:MSVC>>:-Wall -Wextra -Wpedantic>)

enable_testing()

add_test(NAME child_happy_path
  COMMAND logger-child --utc 2026-01-01T00:00:00.000Z --rss-bytes 42 --log-file ${CMAKE_BINARY_DIR}/happy.log)
set_tests_properties(child_happy_path PROPERTIES
  PASS_REGULAR_EXPRESSION "^2026-01-01T00:00:00.000Z rss_bytes=42 elevated=(true|false)")

add_test(NAME child_missing_rss COMMAND logger-child --utc 2026-01-01T00:00:00.000Z)
set_tests_properties(child_missing_rss PROPERTIES WILL_FAIL TRUE)

add_test(NAME child_unknown_flag COMMAND logger-child --utc x --rss-bytes 1 --bogus)
set_tests_properties(child_unknown_flag PROPERTIES WILL_FAIL TRUE)

# A directory as the log file cannot be opened for writing -> exit 2.
add_test(NAME child_unwritable_log COMMAND logger-child --utc x --rss-bytes 1 --log-file ${CMAKE_BINARY_DIR})
set_tests_properties(child_unwritable_log PROPERTIES WILL_FAIL TRUE)

add_test(NAME child_appends_never_truncates
  COMMAND ${CMAKE_COMMAND}
    -DCHILD=$<TARGET_FILE:logger-child>
    -DLOG=${CMAKE_BINARY_DIR}/append.log
    -P ${CMAKE_SOURCE_DIR}/tests/append_test.cmake)
