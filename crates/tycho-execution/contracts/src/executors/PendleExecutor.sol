// SPDX-License-Identifier: BUSL-1.1
pragma solidity ^0.8.26;

import {IExecutor} from "@interfaces/IExecutor.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {TransferManager} from "../TransferManager.sol";

error PendleExecutor__InvalidDataLength();
error PendleExecutor__InvalidEdge();
error PendleExecutor__InvalidTarget();
error PendleExecutor__InvalidRouter();

/// @notice The six edges a Pendle market exposes to a router.
/// @dev The four market edges run through Pendle Router V4; the two SY edges call the SY
/// directly, since minting and redeeming SY is a constant-rate leg that needs no periphery.
enum PendleEdge {
    SyForPt,
    PtForSy,
    SyForYt,
    YtForSy,
    TokenForSy,
    SyForToken
}

contract PendleExecutor is IExecutor {
    /// @dev Pendle Router V4. Immutable rather than caller-supplied: the market in the
    /// calldata already lets the caller pick which market is traded, and there is no reason to
    /// also let it pick which contract is called.
    address public immutable router;

    constructor(address _router) {
        if (_router == address(0)) revert PendleExecutor__InvalidRouter();
        router = _router;
    }

    function fundsExpectedAddress(
        bytes calldata /* data */
    )
        external
        view
        returns (address receiver)
    {
        return msg.sender;
    }

    /// @inheritdoc IExecutor
    /// @dev `minAmountOut` is zero on every call: slippage is enforced once, by the router, on
    /// the final output of the whole solution.
    // slither-disable-next-line locked-ether
    function swap(uint256 amountIn, bytes calldata data, address receiver)
        external
        payable
    {
        (PendleEdge edge, address tokenIn, address tokenOut, address target) =
            _decodeData(data);

        if (edge == PendleEdge.SyForPt) {
            // An all-zero ApproxParams together with an empty LimitOrderData is what makes
            // Router V4 delegate to its on-chain approximation, which derives its own bounds
            // from live market state. Bounds computed at encoding time would be stale by the
            // time the executor runs against an amountIn that drifted.
            IPendleRouterV4(router)
                .swapExactSyForPt(
                    receiver,
                    target,
                    amountIn,
                    0,
                    ApproxParams(0, 0, 0, 0, 0),
                    _emptyLimitOrderData()
                );
        } else if (edge == PendleEdge.PtForSy) {
            IPendleRouterV4(router)
                .swapExactPtForSy(
                    receiver, target, amountIn, 0, _emptyLimitOrderData()
                );
        } else if (edge == PendleEdge.SyForYt) {
            IPendleRouterV4(router)
                .swapExactSyForYt(
                    receiver,
                    target,
                    amountIn,
                    0,
                    ApproxParams(0, 0, 0, 0, 0),
                    _emptyLimitOrderData()
                );
        } else if (edge == PendleEdge.YtForSy) {
            IPendleRouterV4(router)
                .swapExactYtForSy(
                    receiver, target, amountIn, 0, _emptyLimitOrderData()
                );
        } else if (edge == PendleEdge.TokenForSy) {
            if (tokenOut != target) revert PendleExecutor__InvalidTarget();
            // slither-disable-next-line unused-return
            IStandardizedYield(target).deposit(receiver, tokenIn, amountIn, 0);
        } else {
            if (tokenIn != target) revert PendleExecutor__InvalidTarget();
            // slither-disable-next-line unused-return
            IStandardizedYield(target)
                .redeem(receiver, amountIn, tokenOut, 0, false);
        }
    }

    /// @inheritdoc IExecutor
    /// @dev Both Router V4 and the SY pull the input with `transferFrom` and take an explicit
    /// receiver, so every edge is `ProtocolWillDebit` with the output going straight to the
    /// receiver rather than back through the router.
    function getTransferData(bytes calldata data)
        external
        view
        returns (
            TransferManager.TransferType transferType,
            address receiver,
            address tokenIn,
            address tokenOut,
            bool outputToRouter
        )
    {
        PendleEdge edge;
        address target;
        (edge, tokenIn, tokenOut, target) = _decodeData(data);

        receiver = _isMarketEdge(edge) ? router : target;
        transferType = TransferManager.TransferType.ProtocolWillDebit;
        outputToRouter = false;
    }

    function _isMarketEdge(PendleEdge edge) internal pure returns (bool) {
        return edge == PendleEdge.SyForPt || edge == PendleEdge.PtForSy
            || edge == PendleEdge.SyForYt || edge == PendleEdge.YtForSy;
    }

    /// @dev `target` is the market for the four market edges and the SY for the two SY edges.
    function _decodeData(bytes calldata data)
        internal
        pure
        returns (
            PendleEdge edge,
            address tokenIn,
            address tokenOut,
            address target
        )
    {
        if (data.length != 61) {
            revert PendleExecutor__InvalidDataLength();
        }
        uint8 rawEdge = uint8(data[0]);
        if (rawEdge > uint8(type(PendleEdge).max)) {
            revert PendleExecutor__InvalidEdge();
        }
        edge = PendleEdge(rawEdge);
        tokenIn = address(bytes20(data[1:21]));
        tokenOut = address(bytes20(data[21:41]));
        target = address(bytes20(data[41:61]));
    }

    function _emptyLimitOrderData()
        internal
        pure
        returns (LimitOrderData memory limit)
    {}
}

struct ApproxParams {
    uint256 guessMin;
    uint256 guessMax;
    uint256 guessOffchain;
    uint256 maxIteration;
    uint256 eps;
}

/// @dev Pendle's `LimitOrderData` as Router V4 declares it. Only the empty value is ever
/// passed — filling against the limit-order book is out of scope — but the struct still has to
/// be laid out exactly for the calldata to decode.
struct FillOrderParams {
    Order order;
    bytes signature;
    uint256 makingAmount;
}

struct Order {
    uint256 salt;
    uint256 expiry;
    uint256 nonce;
    uint8 orderType;
    address token;
    address YT;
    address maker;
    address receiver;
    uint256 makingAmount;
    uint256 lnImpliedRate;
    uint256 failSafeRate;
    bytes permit;
}

struct LimitOrderData {
    address limitRouter;
    uint256 epsSkipMarket;
    FillOrderParams[] normalFills;
    FillOrderParams[] flashFills;
    bytes optData;
}

interface IPendleRouterV4 {
    function swapExactSyForPt(
        address receiver,
        address market,
        uint256 exactSyIn,
        uint256 minPtOut,
        ApproxParams calldata guessPtOut,
        LimitOrderData calldata limit
    ) external returns (uint256 netPtOut, uint256 netSyFee);

    function swapExactPtForSy(
        address receiver,
        address market,
        uint256 exactPtIn,
        uint256 minSyOut,
        LimitOrderData calldata limit
    ) external returns (uint256 netSyOut, uint256 netSyFee);

    function swapExactSyForYt(
        address receiver,
        address market,
        uint256 exactSyIn,
        uint256 minYtOut,
        ApproxParams calldata guessYtOut,
        LimitOrderData calldata limit
    ) external returns (uint256 netYtOut, uint256 netSyFee);

    function swapExactYtForSy(
        address receiver,
        address market,
        uint256 exactYtIn,
        uint256 minSyOut,
        LimitOrderData calldata limit
    ) external returns (uint256 netSyOut, uint256 netSyFee);
}

interface IStandardizedYield is IERC20 {
    function deposit(
        address receiver,
        address tokenIn,
        uint256 amountTokenToDeposit,
        uint256 minSharesOut
    ) external payable returns (uint256 amountSharesOut);

    function redeem(
        address receiver,
        uint256 amountSharesToRedeem,
        address tokenOut,
        uint256 minTokenOut,
        bool burnFromInternalBalance
    ) external returns (uint256 amountTokenOut);
}
