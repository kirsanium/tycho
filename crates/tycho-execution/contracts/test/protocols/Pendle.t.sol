// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import "../TestUtils.sol";
import "../TychoRouterTestSetup.sol";
import "@src/executors/PendleExecutor.sol";
import {Constants} from "../Constants.sol";

contract PendleExecutorExposed is PendleExecutor {
    constructor(address _router) PendleExecutor(_router) {}

    function decodeParams(bytes calldata data)
        external
        pure
        returns (
            PendleEdge edge,
            address tokenIn,
            address tokenOut,
            address target
        )
    {
        return _decodeData(data);
    }
}

/// Exercises the six edges of the wstETH market expiring 30 Dec 2027 against a mainnet fork.
contract PendleExecutorTest is Constants, TestUtils {
    using SafeERC20 for IERC20;

    PendleExecutorExposed pendleExposed;

    // The per-edge gas the simulation quotes with (`pendle/state.rs`), one constant per
    // direction: the exact-SY-in directions run the router's approximation loop on chain and
    // cost ~30-40% more than their exact-PT counterparts. Each edge is measured in its own test
    // so it runs against a cold fork, which is the state a real swap meets.
    //
    // The measurement covers the executor's `swap` call only. The router's `approve` is added
    // separately by `PROTOCOLS_NEEDING_APPROVAL` in `gas_estimator.rs`, so it must not be here.
    uint256 constant GAS_SY_FOR_PT = 330_000;
    uint256 constant GAS_PT_FOR_SY = 235_000;
    uint256 constant GAS_SY_FOR_YT = 425_000;
    uint256 constant GAS_YT_FOR_SY = 335_000;
    // The wstETH SY is `one_to_one`. An `index_rate` SY reads through to the wrapped protocol
    // and is quoted at GAS_WRAP_INDEX_RATE = 180_000, which no test here measures.
    uint256 constant GAS_WRAP_ONE_TO_ONE = 90_000;

    IERC20 sy = IERC20(PENDLE_WSTETH_SY);
    IERC20 pt = IERC20(PENDLE_WSTETH_PT);
    IERC20 yt = IERC20(PENDLE_WSTETH_YT);
    IERC20 wstETH = IERC20(WSTETH_ADDR);

    function setUp() public {
        uint256 forkBlock = 25800000;
        vm.createSelectFork(vm.rpcUrl("mainnet"), forkBlock);
        pendleExposed = new PendleExecutorExposed(PENDLE_ROUTER_V4);
    }

    function _data(
        PendleEdge edge,
        address tokenIn,
        address tokenOut,
        address target
    ) internal pure returns (bytes memory) {
        return abi.encodePacked(uint8(edge), tokenIn, tokenOut, target);
    }

    /// Funds the executor and gives the contract that will pull the tokens an allowance, which
    /// is what TransferManager does for a `ProtocolWillDebit` executor before delegatecalling.
    function _fund(address token, uint256 amount, address spender) internal {
        deal(token, address(pendleExposed), amount);
        vm.prank(address(pendleExposed));
        IERC20(token).forceApprove(spender, amount);
    }

    function testDecodeParams() public view {
        bytes memory params = _data(
            PendleEdge.SyForPt,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_MARKET
        );

        (PendleEdge edge, address tokenIn, address tokenOut, address target) =
            pendleExposed.decodeParams(params);

        assertEq(uint8(edge), uint8(PendleEdge.SyForPt));
        assertEq(tokenIn, PENDLE_WSTETH_SY);
        assertEq(tokenOut, PENDLE_WSTETH_PT);
        assertEq(target, PENDLE_WSTETH_MARKET);
    }

    function testDecodeParamsInvalidDataLength() public {
        bytes memory params = abi.encodePacked(
            uint8(0),
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_MARKET,
            uint8(0)
        );

        vm.expectRevert(PendleExecutor__InvalidDataLength.selector);
        pendleExposed.decodeParams(params);
    }

    function testDecodeParamsInvalidEdge() public {
        bytes memory params = abi.encodePacked(
            uint8(6), PENDLE_WSTETH_SY, PENDLE_WSTETH_PT, PENDLE_WSTETH_MARKET
        );

        vm.expectRevert(PendleExecutor__InvalidEdge.selector);
        pendleExposed.decodeParams(params);
    }

    function testGetTransferDataMarketEdge() public view {
        bytes memory params = _data(
            PendleEdge.PtForSy,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_MARKET
        );

        (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        ) = pendleExposed.getTransferData(params);

        assertEq(
            uint8(transferType),
            uint8(TransferManager.TransferType.ProtocolWillDebit)
        );
        // The market edges are pulled by Router V4, not by the market itself.
        assertEq(receiver, PENDLE_ROUTER_V4);
        assertEq(tokenIn, PENDLE_WSTETH_PT);
        assertEq(tokenOut, PENDLE_WSTETH_SY);
        assertEq(outputToRouter, false);
    }

    function testGetTransferDataSyEdge() public view {
        bytes memory params = _data(
            PendleEdge.TokenForSy,
            WSTETH_ADDR,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY
        );

        (, address receiver, address tokenIn, address tokenOut,) =
            pendleExposed.getTransferData(params);

        assertEq(receiver, PENDLE_WSTETH_SY);
        assertEq(tokenIn, WSTETH_ADDR);
        assertEq(tokenOut, PENDLE_WSTETH_SY);
    }

    /// `SY.redeem` burns the caller's shares without `transferFrom`, so a redemption names the
    /// caller, which the Dispatcher does not approve, rather than the SY.
    function testGetTransferDataRedeemNeedsNoApproval() public {
        bytes memory params = _data(
            PendleEdge.SyForToken,
            PENDLE_WSTETH_SY,
            WSTETH_ADDR,
            PENDLE_WSTETH_SY
        );

        vm.prank(ALICE);
        (, address receiver,,,) = pendleExposed.getTransferData(params);

        assertEq(receiver, ALICE);
    }

    function testSwapSyForTokenWithoutAllowance() public {
        deal(PENDLE_WSTETH_SY, address(pendleExposed), 1e18);
        uint256 balanceBefore = wstETH.balanceOf(BOB);

        pendleExposed.swap(
            1e18,
            _data(
                PendleEdge.SyForToken,
                PENDLE_WSTETH_SY,
                WSTETH_ADDR,
                PENDLE_WSTETH_SY
            ),
            BOB
        );

        assertGt(wstETH.balanceOf(BOB), balanceBefore);
        assertEq(sy.balanceOf(address(pendleExposed)), 0);
    }

    function testSwapSyForPt() public {
        uint256 balanceBefore = pt.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.SyForPt,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_MARKET,
            PENDLE_ROUTER_V4
        );

        assertGt(pt.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, GAS_SY_FOR_PT);
    }

    function testSwapPtForSy() public {
        uint256 balanceBefore = sy.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.PtForSy,
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_MARKET,
            PENDLE_ROUTER_V4
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, GAS_PT_FOR_SY);
    }

    function testSwapSyForYt() public {
        uint256 balanceBefore = yt.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.SyForYt,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_YT,
            PENDLE_WSTETH_MARKET,
            PENDLE_ROUTER_V4
        );

        assertGt(yt.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, GAS_SY_FOR_YT);
    }

    function testSwapYtForSy() public {
        uint256 balanceBefore = sy.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.YtForSy,
            PENDLE_WSTETH_YT,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_MARKET,
            PENDLE_ROUTER_V4
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, GAS_YT_FOR_SY);
    }

    function testSwapTokenForSy() public {
        uint256 balanceBefore = sy.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.TokenForSy,
            WSTETH_ADDR,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, GAS_WRAP_ONE_TO_ONE);
    }

    function testSwapSyForToken() public {
        uint256 balanceBefore = wstETH.balanceOf(BOB);

        uint256 gasUsed = _measure(
            PendleEdge.SyForToken,
            PENDLE_WSTETH_SY,
            WSTETH_ADDR,
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_SY
        );

        assertGt(wstETH.balanceOf(BOB), balanceBefore);
        assertLe(gasUsed, GAS_WRAP_ONE_TO_ONE);
    }

    /// Funds the executor, runs one edge, and returns the gas the `swap` call itself consumed.
    function _measure(
        PendleEdge edge,
        address tokenIn,
        address tokenOut,
        address target,
        address spender
    ) internal returns (uint256 gasUsed) {
        uint256 amountIn = 1e18;
        _fund(tokenIn, amountIn, spender);
        bytes memory data = _data(edge, tokenIn, tokenOut, target);

        uint256 before = gasleft();
        pendleExposed.swap(amountIn, data, BOB);
        gasUsed = before - gasleft();
        console.log("pendle edge gas", uint8(edge), gasUsed);
    }

    function testSwapTokenForSyRejectsMismatchedTarget() public {
        // The SY edges call the token in the calldata; a target that is not the SY being
        // minted has to fail rather than call an arbitrary contract.
        vm.expectRevert(PendleExecutor__InvalidTarget.selector);
        pendleExposed.swap(
            1e18,
            _data(
                PendleEdge.TokenForSy,
                WSTETH_ADDR,
                PENDLE_WSTETH_SY,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );
    }

    /// The exit direction carries the same check, against `tokenIn` rather than `tokenOut`.
    function testSwapSyForTokenRejectsMismatchedTarget() public {
        vm.expectRevert(PendleExecutor__InvalidTarget.selector);
        pendleExposed.swap(
            1e18,
            _data(
                PendleEdge.SyForToken,
                PENDLE_WSTETH_SY,
                WSTETH_ADDR,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );
    }

    /// Calldata the encoder would never produce: native ETH, by the router's sentinel or the zero
    /// address an SY may list, fails at the decode with a typed error.
    function testDecodeParamsRejectsNativeEth() public {
        address[2] memory natives =
            [address(0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE), address(0)];
        for (uint256 i = 0; i < natives.length; i++) {
            vm.expectRevert(PendleExecutor__NativeEthUnsupported.selector);
            pendleExposed.decodeParams(
                _data(
                    PendleEdge.TokenForSy,
                    natives[i],
                    PENDLE_WSTETH_SY,
                    PENDLE_WSTETH_SY
                )
            );
            vm.expectRevert(PendleExecutor__NativeEthUnsupported.selector);
            pendleExposed.decodeParams(
                _data(
                    PendleEdge.SyForToken,
                    PENDLE_WSTETH_SY,
                    natives[i],
                    PENDLE_WSTETH_SY
                )
            );
        }
    }

    /// The router is immutable, so a zero address would leave every market edge calling nothing
    /// with no way to correct it.
    function testConstructorRejectsZeroRouter() public {
        vm.expectRevert(PendleExecutor__InvalidRouter.selector);
        new PendleExecutor(address(0));
    }
}

/// Runs the calldata the Rust encoder produced through the deployed router, for every edge.
contract TychoRouterPendleTest is TychoRouterTestSetup {
    function getForkBlock() public pure override returns (uint256) {
        return 25800000;
    }

    /// Sends `amountIn` of `tokenIn` through the router with the calldata generated by the
    /// Rust test of the same name, and returns what ALICE received.
    function _runEdge(
        string memory testName,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) internal returns (uint256 received) {
        deal(tokenIn, ALICE, amountIn);
        uint256 balanceBefore = IERC20(tokenOut).balanceOf(ALICE);

        vm.startPrank(ALICE);
        IERC20(tokenIn).approve(tychoRouterAddr, amountIn);
        (bool success,) = tychoRouterAddr.call(loadCallDataFromFile(testName));
        vm.stopPrank();

        assertTrue(success, "Call Failed");
        received = IERC20(tokenOut).balanceOf(ALICE) - balanceBefore;
    }

    function testRouterSyForPt() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_sy_for_pt",
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_PT,
            1e18
        );
        assertEq(received, 1276552648088633757);
    }

    function testRouterPtForSy() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_pt_for_sy",
            PENDLE_WSTETH_PT,
            PENDLE_WSTETH_SY,
            1e18
        );
        assertEq(received, 781959205515422198);
    }

    function testRouterSyForYt() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_sy_for_yt",
            PENDLE_WSTETH_SY,
            PENDLE_WSTETH_YT,
            1e18
        );
        assertEq(received, 36536075805370490884);
    }

    function testRouterYtForSy() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_yt_for_sy",
            PENDLE_WSTETH_YT,
            PENDLE_WSTETH_SY,
            1e18
        );
        assertEq(received, 21770141148458769);
    }

    function testRouterTokenForSy() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_token_for_sy",
            WSTETH_ADDR,
            PENDLE_WSTETH_SY,
            1e18
        );
        assertEq(received, 1000000000000000000);
    }

    function testRouterSyForToken() public {
        uint256 received = _runEdge(
            "test_single_encoding_strategy_pendle_sy_for_token",
            PENDLE_WSTETH_SY,
            WSTETH_ADDR,
            1e18
        );
        assertEq(received, 1000000000000000000);
    }
}
