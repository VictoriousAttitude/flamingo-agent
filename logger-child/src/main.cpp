// logger-child: receives the agent's metrics as arguments, writes one line to stdout and
// appends the same line to the log file, then exits. The 5-second cadence lives in the agent,
// which spawns a fresh child every cycle.
//
// The log file is created and locked down by the agent before this program runs. This
// program only appends: it never truncates, deletes or recreates the file, because a
// recreated file would lose the agent's ACL and inherit default permissions instead.

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#define _CRT_SECURE_NO_WARNINGS
#include <windows.h>
#else
#include <unistd.h>
#endif

#include <cstdlib>
#include <fstream>
#include <iostream>
#include <string>

namespace {

// Whether this process actually runs with elevated privileges. Reported in every log line so
// the "launched with administrator privileges" requirement has evidence, not just an assertion.
bool is_elevated() {
#ifdef _WIN32
    HANDLE token = nullptr;
    if (!OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &token)) {
        return false;
    }
    TOKEN_ELEVATION elevation{};
    DWORD returned = 0;
    const bool ok = GetTokenInformation(token, TokenElevation, &elevation, sizeof elevation, &returned) != 0;
    CloseHandle(token);
    return ok && elevation.TokenIsElevated != 0;
#else
    return geteuid() == 0;
#endif
}

std::string default_log_path() {
#ifdef _WIN32
    const char* program_data = std::getenv("ProgramData");
    const std::string base = program_data != nullptr ? program_data : "C:\\ProgramData";
    return base + "\\FlamingoAgent\\child.log";
#else
    return "/var/log/flamingo-agent/child.log";
#endif
}

struct Options {
    std::string utc;
    std::string rss_bytes;
    std::string log_file;
};

bool parse(int argc, char** argv, Options& options, std::string& error) {
    for (int i = 1; i < argc; ++i) {
        const std::string arg = argv[i];
        const bool has_value = i + 1 < argc;
        if (arg == "--utc" && has_value) {
            options.utc = argv[++i];
        } else if (arg == "--rss-bytes" && has_value) {
            options.rss_bytes = argv[++i];
        } else if (arg == "--log-file" && has_value) {
            options.log_file = argv[++i];
        } else {
            error = "unknown or incomplete argument: " + arg;
            return false;
        }
    }
    if (options.utc.empty() || options.rss_bytes.empty()) {
        error = "--utc and --rss-bytes are required";
        return false;
    }
    if (options.log_file.empty()) {
        options.log_file = default_log_path();
    }
    return true;
}

}  // namespace

int main(int argc, char** argv) {
    Options options;
    std::string error;
    if (!parse(argc, argv, options, error)) {
        std::cerr << "logger-child: " << error << '\n'
                  << "usage: logger-child --utc <rfc3339> --rss-bytes <n> [--log-file <path>]\n";
        return 1;
    }

    const std::string line =
        options.utc + " rss_bytes=" + options.rss_bytes + " elevated=" + (is_elevated() ? "true" : "false");

    std::cout << line << std::endl;

    std::ofstream out(options.log_file, std::ios::app);
    if (!out) {
        std::cerr << "logger-child: cannot open log file for append: " << options.log_file << '\n';
        return 2;
    }
    out << line << '\n';
    out.flush();
    if (!out) {
        std::cerr << "logger-child: write failed: " << options.log_file << '\n';
        return 2;
    }
    return 0;
}
