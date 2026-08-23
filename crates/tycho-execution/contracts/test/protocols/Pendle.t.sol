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

    function testSwapSyForPt() public {
        uint256 amountIn = 1e18;
        _fund(PENDLE_WSTETH_SY, amountIn, PENDLE_ROUTER_V4);

        uint256 balanceBefore = pt.balanceOf(BOB);
        pendleExposed.swap(
            amountIn,
            _data(
                PendleEdge.SyForPt,
                PENDLE_WSTETH_SY,
                PENDLE_WSTETH_PT,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );

        assertGt(pt.balanceOf(BOB), balanceBefore);
    }

    function testSwapPtForSy() public {
        uint256 amountIn = 1e18;
        _fund(PENDLE_WSTETH_PT, amountIn, PENDLE_ROUTER_V4);

        uint256 balanceBefore = sy.balanceOf(BOB);
        pendleExposed.swap(
            amountIn,
            _data(
                PendleEdge.PtForSy,
                PENDLE_WSTETH_PT,
                PENDLE_WSTETH_SY,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
    }

    function testSwapSyForYt() public {
        uint256 amountIn = 1e18;
        _fund(PENDLE_WSTETH_SY, amountIn, PENDLE_ROUTER_V4);

        uint256 balanceBefore = yt.balanceOf(BOB);
        pendleExposed.swap(
            amountIn,
            _data(
                PendleEdge.SyForYt,
                PENDLE_WSTETH_SY,
                PENDLE_WSTETH_YT,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );

        assertGt(yt.balanceOf(BOB), balanceBefore);
    }

    function testSwapYtForSy() public {
        uint256 amountIn = 1e18;
        _fund(PENDLE_WSTETH_YT, amountIn, PENDLE_ROUTER_V4);

        uint256 balanceBefore = sy.balanceOf(BOB);
        pendleExposed.swap(
            amountIn,
            _data(
                PendleEdge.YtForSy,
                PENDLE_WSTETH_YT,
                PENDLE_WSTETH_SY,
                PENDLE_WSTETH_MARKET
            ),
            BOB
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
    }

    function testSwapTokenForSy() public {
        uint256 amountIn = 1e18;
        _fund(WSTETH_ADDR, amountIn, PENDLE_WSTETH_SY);

        uint256 balanceBefore = sy.balanceOf(BOB);
        pendleExposed.swap(
            amountIn,
            _data(
                PendleEdge.TokenForSy,
                WSTETH_ADDR,
                PENDLE_WSTETH_SY,
                PENDLE_WSTETH_SY
            ),
            BOB
        );

        assertGt(sy.balanceOf(BOB), balanceBefore);
    }

    function testSwapSyForToken() public {
        uint256 amountIn = 1e18;
        // `redeem` burns the shares the SY holds for the caller, so no allowance is needed.
        deal(PENDLE_WSTETH_SY, address(pendleExposed), amountIn);

        uint256 balanceBefore = wstETH.balanceOf(BOB);
        pendleExposed.swap(
            amountIn,
            _data(
                PendleEdge.SyForToken,
                PENDLE_WSTETH_SY,
                WSTETH_ADDR,
                PENDLE_WSTETH_SY
            ),
            BOB
        );

        assertGt(wstETH.balanceOf(BOB), balanceBefore);
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
}
